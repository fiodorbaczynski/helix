//! Workspace file watcher for external-change buffer reloads.
//!
//! Helix watches the workspace it was launched in (cwd containing a `.git`
//! entry) and feeds filesystem events into the editor to reload open buffers
//! and drive LSP `DidChangeWatchedFiles` fan-out.
//!
//! Currently macOS-only; on other platforms [`FileWatcher::start`] returns
//! `None` and the editor behaves as before.

use std::path::{Path, PathBuf};

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
    use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

    use helix_lsp::file_event;

    /// Holds the live FSEvents stream for the workspace.
    ///
    /// Dropping this value stops the stream: the underlying `Watcher` is
    /// dropped, its sync sender disconnects, and the bridging blocking task
    /// exits when `recv` returns `Err`.
    pub struct FileWatcher {
        _watcher: RecommendedWatcher,
    }

    impl FileWatcher {
        pub fn start(root: PathBuf, lsp_fan_out: file_event::Handler) -> Option<Self> {
            // FSEvents returns canonical paths (e.g. `/private/var/...` on
            // macOS). The gitignore matcher panics in debug builds when given
            // a path that isn't under its configured root, so we canonicalize
            // the root up front to keep the two forms aligned.
            let root = std::fs::canonicalize(&root).unwrap_or(root);

            let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

            let mut watcher = match RecommendedWatcher::new(tx, Config::default()) {
                Ok(watcher) => watcher,
                Err(err) => {
                    log::warn!("file watcher unavailable: {err}");
                    return None;
                }
            };

            if let Err(err) = watcher.watch(&root, RecursiveMode::Recursive) {
                log::warn!("failed to watch workspace {}: {err}", root.display());
                return None;
            }

            let ignore = build_ignore(&root);

            log::info!("watching workspace for external changes: {}", root.display());

            tokio::task::spawn_blocking(move || drain(rx, lsp_fan_out, ignore));

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
        ignore: Gitignore,
    ) {
        while let Ok(result) = rx.recv() {
            match result {
                Ok(event) => dispatch(&event, &lsp_fan_out, &ignore),
                Err(err) => log::warn!("file watcher error: {err}"),
            }
        }
    }

    /// Routes a single notify event to the LSP fan-out registry after filtering.
    fn dispatch(event: &Event, lsp_fan_out: &file_event::Handler, ignore: &Gitignore) {
        // Access events never change content; dropping them early avoids a
        // stat-per-event under heavy read traffic.
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }

        for path in &event.paths {
            if is_ignored(ignore, path) {
                log::trace!("file watcher: ignored {}", path.display());
                continue;
            }
            log::trace!("file watcher: forwarding {}", path.display());
            lsp_fan_out.file_changed(path.clone());
        }
    }

    fn is_ignored(ignore: &Gitignore, path: &Path) -> bool {
        ignore
            .matched_path_or_any_parents(path, path.is_dir())
            .is_ignore()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::PathBuf;

    pub struct FileWatcher;

    impl FileWatcher {
        pub fn start(
            _root: PathBuf,
            _lsp_fan_out: helix_lsp::file_event::Handler,
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
