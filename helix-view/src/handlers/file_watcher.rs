//! Workspace file watcher for external-change buffer reloads.
//!
//! Helix watches the workspace it was launched in (cwd containing a `.git`
//! entry) and feeds filesystem events into the editor to reload open buffers
//! and drive LSP `DidChangeWatchedFiles` fan-out.
//!
//! Currently macOS-only; on other platforms [`FileWatcher::start`] returns
//! `None` and the editor behaves as before.

use std::path::{Path, PathBuf};

/// An external filesystem event that may require the editor to update its
/// in-memory state. The watcher emits these; the editor consumes them on its
/// main event loop.
#[derive(Debug, Clone)]
pub enum FileWatcherEvent {
    /// The file's content was likely modified (created, written, or replaced).
    /// Consumers should reload any buffer whose path matches.
    Modified(PathBuf),
    /// The file was removed from disk. Consumers should close any clean
    /// buffer whose path matches; dirty buffers should be flagged so the
    /// user notices their disk state has gone away.
    Removed(PathBuf),
}

/// Returns the workspace root if `cwd` is itself a workspace.
///
/// A directory is a workspace iff it directly contains a `.git` entry — regular
/// directory, submodule pointer file, or symlink all qualify. No walk-up, no
/// other heuristics: helix watches what the user explicitly pointed it at.
pub fn workspace_root(cwd: &Path) -> Option<PathBuf> {
    cwd.join(".git")
        .try_exists()
        .ok()
        .and_then(|exists| exists.then(|| cwd.to_path_buf()))
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    use ignore::gitignore::{Gitignore, GitignoreBuilder};
    use notify::event::ModifyKind;
    use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    use tokio::sync::mpsc::UnboundedSender;

    use helix_lsp::file_event;

    use super::FileWatcherEvent;

    /// Holds the live FSEvents stream for the workspace.
    ///
    /// Dropping this value stops the stream: the underlying `Watcher` is
    /// dropped, its sync sender disconnects, and the bridging blocking task
    /// exits when `recv` returns `Err`.
    pub struct FileWatcher {
        _watcher: RecommendedWatcher,
    }

    impl FileWatcher {
        pub fn start(
            root: PathBuf,
            lsp_fan_out: file_event::Handler,
            editor_tx: UnboundedSender<FileWatcherEvent>,
        ) -> Option<Self> {
            // helix stores document paths via `helix_stdx::path::canonicalize`,
            // which normalises `..`/`.` but does not resolve symlinks.
            // FSEvents, by contrast, reports events with the symlink-resolved
            // path (e.g. `/private/var/...` instead of `/var/...` on macOS).
            // We need both forms: the FS form to pass to notify and to match
            // ignore rules against, and the helix form to look up open
            // documents. Event paths are rewritten from the former to the
            // latter at dispatch time.
            let helix_root = helix_stdx::path::canonicalize(&root);
            let fs_root = std::fs::canonicalize(&root).unwrap_or_else(|_| helix_root.clone());

            let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

            let mut watcher = match RecommendedWatcher::new(tx, Config::default()) {
                Ok(watcher) => watcher,
                Err(err) => {
                    log::warn!("file watcher unavailable: {err}");
                    return None;
                }
            };

            if let Err(err) = watcher.watch(&fs_root, RecursiveMode::Recursive) {
                log::warn!(
                    "failed to watch workspace {}: {err}",
                    fs_root.display()
                );
                return None;
            }

            let ignore = build_ignore(&fs_root);

            log::info!(
                "watching workspace for external changes: {}",
                fs_root.display()
            );

            tokio::task::spawn_blocking(move || {
                drain(rx, lsp_fan_out, editor_tx, ignore, fs_root, helix_root)
            });

            Some(Self { _watcher: watcher })
        }
    }

    /// Build a gitignore matcher for the workspace root.
    ///
    /// Only root-level ignore files are loaded — nested `.gitignore`s in
    /// subdirectories are not consulted. This is good enough for the common
    /// cases (build outputs, vendored deps) and keeps startup cheap on large
    /// trees. `.git/` is added unconditionally so we never forward events from
    /// inside the git directory.
    fn build_ignore(root: &Path) -> Gitignore {
        let mut builder = GitignoreBuilder::new(root);
        for name in [".gitignore", ".ignore", ".helix/ignore"] {
            let path = root.join(name);
            if path.exists() {
                if let Some(err) = builder.add(path) {
                    log::warn!("ignore file for file watcher: {err}");
                }
            }
        }
        if let Err(err) = builder.add_line(None, ".git/") {
            log::warn!("ignore rule for file watcher: {err}");
        }
        builder.build().unwrap_or_else(|err| {
            log::warn!("gitignore build failed, proceeding without ignore rules: {err}");
            Gitignore::empty()
        })
    }

    fn drain(
        rx: mpsc::Receiver<notify::Result<Event>>,
        lsp_fan_out: file_event::Handler,
        editor_tx: UnboundedSender<FileWatcherEvent>,
        ignore: Gitignore,
        fs_root: PathBuf,
        helix_root: PathBuf,
    ) {
        while let Ok(result) = rx.recv() {
            match result {
                Ok(event) => dispatch(
                    &event,
                    &lsp_fan_out,
                    &editor_tx,
                    &ignore,
                    &fs_root,
                    &helix_root,
                ),
                Err(err) => log::warn!("file watcher error: {err}"),
            }
        }
    }

    /// Routes a single notify event to the LSP fan-out registry and, when
    /// the event's kind has buffer-level semantics, also to the editor.
    fn dispatch(
        event: &Event,
        lsp_fan_out: &file_event::Handler,
        editor_tx: &UnboundedSender<FileWatcherEvent>,
        ignore: &Gitignore,
        fs_root: &Path,
        helix_root: &Path,
    ) {
        // Access events never change content; dropping them early avoids a
        // stat-per-event under heavy read traffic.
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }

        // The LSP fan-out forwards every non-access event (metadata matters
        // for some servers), but the editor only cares about kinds with
        // observable buffer-level semantics. Renames are deferred to a later
        // commit; metadata-only changes never warrant a reload.
        let make_editor_event: Option<fn(PathBuf) -> FileWatcherEvent> = match event.kind {
            EventKind::Modify(ModifyKind::Metadata(_)) => None,
            EventKind::Modify(ModifyKind::Name(_)) => None,
            EventKind::Remove(_) => Some(FileWatcherEvent::Removed),
            _ => Some(FileWatcherEvent::Modified),
        };

        for path in &event.paths {
            // Ignore matching is done against the FS-form path because the
            // matcher was built with `fs_root`.
            if is_ignored(ignore, path) {
                log::trace!("file watcher: ignored {}", path.display());
                continue;
            }
            // FSEvents replays historical events on workspace open. A Remove
            // for a path that still exists is a stale replay — drop it so we
            // don't tell consumers a live file vanished.
            if matches!(event.kind, EventKind::Remove(_)) && path.exists() {
                log::trace!("file watcher: dropping stale Remove for {}", path.display());
                continue;
            }
            let remapped = remap_to_helix_form(path, fs_root, helix_root);
            log::trace!("file watcher: forwarding {}", remapped.display());
            lsp_fan_out.file_changed(remapped.clone());
            if let Some(make_event) = make_editor_event {
                // `send` only fails if the receiver has been dropped, which
                // only happens at editor shutdown — losing an event in flight
                // then is fine.
                let _ = editor_tx.send(make_event(remapped));
            }
        }
    }

    fn is_ignored(ignore: &Gitignore, path: &Path) -> bool {
        ignore
            .matched_path_or_any_parents(path, path.is_dir())
            .is_ignore()
    }

    /// Rewrite a path from the FSEvents-reported form (symlinks resolved) to
    /// the form helix uses for document bookkeeping (symlinks preserved).
    /// Paths that don't start with `fs_root` are returned unchanged — they're
    /// outside the watched tree, which shouldn't normally happen but isn't
    /// worth crashing over.
    fn remap_to_helix_form(path: &Path, fs_root: &Path, helix_root: &Path) -> PathBuf {
        match path.strip_prefix(fs_root) {
            Ok(rel) => helix_root.join(rel),
            Err(_) => path.to_path_buf(),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{FileWatcherEvent, PathBuf};
    use tokio::sync::mpsc::UnboundedSender;

    pub struct FileWatcher;

    impl FileWatcher {
        pub fn start(
            _root: PathBuf,
            _lsp_fan_out: helix_lsp::file_event::Handler,
            _editor_tx: UnboundedSender<FileWatcherEvent>,
        ) -> Option<Self> {
            None
        }
    }
}

pub use imp::FileWatcher;

#[cfg(test)]
mod tests {
    use super::workspace_root;
    use std::fs;

    #[test]
    fn detects_git_directory_as_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        assert_eq!(workspace_root(tmp.path()).as_deref(), Some(tmp.path()));
    }

    #[test]
    fn detects_git_file_as_workspace() {
        // Submodules and worktrees use a `.git` file pointing at the real gitdir.
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(workspace_root(tmp.path()).as_deref(), Some(tmp.path()));
    }

    #[test]
    fn rejects_non_workspace_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(workspace_root(tmp.path()), None);
    }
}
