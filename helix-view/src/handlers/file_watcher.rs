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
    /// The file was renamed from `from` to `to` within the watched tree.
    /// Consumers should redirect any buffer at `from` to `to` so the
    /// editor's view follows the file. Renames that cross the watched
    /// boundary in either direction never produce this variant — the
    /// watcher emits `Removed` (renamed out) or `Modified` (renamed in).
    Renamed { from: PathBuf, to: PathBuf },
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
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    use ignore::gitignore::{Gitignore, GitignoreBuilder};
    use ignore::{Match, WalkBuilder};
    use notify::event::ModifyKind;
    use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    use tokio::sync::mpsc::UnboundedSender;

    use helix_lsp::file_event;

    use super::FileWatcherEvent;

    /// FSEvents on macOS reports the two ends of a rename as separate
    /// `Modify(Name(Any))` events, with no built-in pairing. We buffer one
    /// half and wait briefly for its counterpart; if no counterpart arrives
    /// the half is flushed as a `Removed` (source) or `Modified`
    /// (destination) — that covers renames crossing the workspace boundary.
    const RENAME_PAIRING_WINDOW: Duration = Duration::from_millis(100);

    /// One half of an in-flight rename. `is_source` is determined by the
    /// path's existence at receipt: source paths no longer exist, target
    /// paths do.
    struct PendingRename {
        helix_path: PathBuf,
        received_at: Instant,
        is_source: bool,
    }

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

    /// A collection of per-directory `Gitignore` matchers covering the
    /// workspace tree, queried hierarchically (deepest matcher wins).
    ///
    /// A single `Gitignore` rooted at the workspace top doesn't honour
    /// nested gitignore scoping correctly: patterns from `/sub/.gitignore`
    /// would be matched against the workspace root rather than against
    /// `/sub`. Storing one matcher per gitignore-bearing directory keeps
    /// patterns scoped to where they were declared.
    struct HierarchicalIgnore {
        matchers: HashMap<PathBuf, Gitignore>,
        root: PathBuf,
    }

    impl HierarchicalIgnore {
        /// Returns true if the given path is ignored. Walks up from the
        /// path's parent through the workspace root, querying each
        /// directory that has a matcher; the first non-`None` result wins.
        fn matches(&self, path: &Path) -> bool {
            let mut current = path.parent();
            while let Some(dir) = current {
                if let Some(matcher) = self.matchers.get(dir) {
                    match matcher.matched_path_or_any_parents(path, path.is_dir()) {
                        Match::Ignore(_) => return true,
                        Match::Whitelist(_) => return false,
                        Match::None => {}
                    }
                }
                if dir == self.root {
                    break;
                }
                current = dir.parent();
            }
            false
        }
    }

    /// Build the hierarchical ignore matcher for the workspace.
    ///
    /// Loads `.gitignore` and `.ignore` from every directory under `root`
    /// (skipping subtrees that ignore rules already exclude), plus the
    /// workspace-level `.helix/ignore` at the root. `.git/` is added
    /// unconditionally so we never forward events from inside the git
    /// directory.
    fn build_ignore(root: &Path) -> HierarchicalIgnore {
        let mut builders: HashMap<PathBuf, GitignoreBuilder> = HashMap::new();

        // Root entry: standard ignore files plus the workspace-level
        // `.helix/ignore` (Helix convention) and the unconditional .git/
        // block. `.helix/ignore` is workspace-wide, not per-directory.
        let root_builder = builders
            .entry(root.to_path_buf())
            .or_insert_with(|| GitignoreBuilder::new(root));
        for name in [".gitignore", ".ignore", ".helix/ignore"] {
            let path = root.join(name);
            if path.exists() {
                if let Some(err) = root_builder.add(&path) {
                    log::warn!("ignore file {}: {err}", path.display());
                }
            }
        }
        if let Err(err) = root_builder.add_line(None, ".git/") {
            log::warn!("ignore rule for .git/: {err}");
        }

        // `WalkBuilder::hidden(false)` is required so the walker yields
        // dotfiles — including the `.gitignore` files we're trying to
        // collect. The walker itself respects the ignore rules it
        // discovers, so subtrees pruned by an ancestor's rules are skipped
        // automatically.
        let walker = WalkBuilder::new(root).hidden(false).build();
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    log::warn!("file watcher walk: {err}");
                    continue;
                }
            };
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name,
                None => continue,
            };
            if name != ".gitignore" && name != ".ignore" {
                continue;
            }
            let parent = match path.parent() {
                Some(parent) => parent,
                None => continue,
            };
            // Root-level files were already added above.
            if parent == root {
                continue;
            }
            let builder = builders
                .entry(parent.to_path_buf())
                .or_insert_with(|| GitignoreBuilder::new(parent));
            if let Some(err) = builder.add(path) {
                log::warn!("nested ignore {}: {err}", path.display());
            }
        }

        let matchers = builders
            .into_iter()
            .filter_map(|(dir, builder)| match builder.build() {
                Ok(matcher) => Some((dir, matcher)),
                Err(err) => {
                    log::warn!("gitignore build for {} failed: {err}", dir.display());
                    None
                }
            })
            .collect();

        HierarchicalIgnore {
            matchers,
            root: root.to_path_buf(),
        }
    }

    fn drain(
        rx: mpsc::Receiver<notify::Result<Event>>,
        lsp_fan_out: file_event::Handler,
        editor_tx: UnboundedSender<FileWatcherEvent>,
        ignore: HierarchicalIgnore,
        fs_root: PathBuf,
        helix_root: PathBuf,
    ) {
        let mut pending: Option<PendingRename> = None;

        loop {
            // While a half-rename is buffered, only block until its window
            // expires; otherwise wait indefinitely for the next event.
            let received = match pending.as_ref() {
                Some(p) => {
                    let remaining = RENAME_PAIRING_WINDOW.saturating_sub(p.received_at.elapsed());
                    rx.recv_timeout(remaining)
                }
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };

            match received {
                Ok(Ok(event)) => dispatch(
                    &event,
                    &mut pending,
                    &lsp_fan_out,
                    &editor_tx,
                    &ignore,
                    &fs_root,
                    &helix_root,
                ),
                Ok(Err(err)) => log::warn!("file watcher error: {err}"),
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(p) = pending.take() {
                        flush_orphan(p, &editor_tx);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Routes a single notify event to the LSP fan-out registry and, when
    /// the event's kind has buffer-level semantics, also to the editor.
    fn dispatch(
        event: &Event,
        pending: &mut Option<PendingRename>,
        lsp_fan_out: &file_event::Handler,
        editor_tx: &UnboundedSender<FileWatcherEvent>,
        ignore: &HierarchicalIgnore,
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
        // observable buffer-level semantics. Name events are routed through
        // the rename pairing layer; metadata-only changes never warrant a
        // reload.
        let is_name = matches!(event.kind, EventKind::Modify(ModifyKind::Name(_)));
        let make_editor_event: Option<fn(PathBuf) -> FileWatcherEvent> = match event.kind {
            EventKind::Modify(ModifyKind::Metadata(_)) => None,
            EventKind::Modify(ModifyKind::Name(_)) => None,
            EventKind::Remove(_) => Some(FileWatcherEvent::Removed),
            _ => Some(FileWatcherEvent::Modified),
        };

        for path in &event.paths {
            // Ignore matching is done against the FS-form path because the
            // matchers were built rooted in `fs_root` (and its descendants).
            if ignore.matches(path) {
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

            if is_name {
                let is_source = !path.exists();
                let arrived = PendingRename {
                    helix_path: remapped,
                    received_at: Instant::now(),
                    is_source,
                };
                *pending = pair_or_buffer(arrived, pending.take(), editor_tx);
            } else if let Some(make_event) = make_editor_event {
                // `send` only fails if the receiver has been dropped, which
                // only happens at editor shutdown — losing an event in flight
                // then is fine.
                let _ = editor_tx.send(make_event(remapped));
            }
        }
    }

    /// Try to pair a freshly-arrived Name half with whatever was buffered.
    ///
    /// Outcomes:
    /// - opposite-side pair: emit `Renamed` and clear the slot;
    /// - same-side collision: flush the older half as an orphan and keep
    ///   the new one (the older one's counterpart isn't coming);
    /// - empty slot: store the new half.
    fn pair_or_buffer(
        arrived: PendingRename,
        previous: Option<PendingRename>,
        editor_tx: &UnboundedSender<FileWatcherEvent>,
    ) -> Option<PendingRename> {
        match previous {
            Some(prev) if prev.is_source != arrived.is_source => {
                let (from, to) = if prev.is_source {
                    (prev.helix_path, arrived.helix_path)
                } else {
                    (arrived.helix_path, prev.helix_path)
                };
                let _ = editor_tx.send(FileWatcherEvent::Renamed { from, to });
                None
            }
            Some(prev) => {
                flush_orphan(prev, editor_tx);
                Some(arrived)
            }
            None => Some(arrived),
        }
    }

    /// Emit a buffered half-rename as if it were a one-sided event. A
    /// stranded source means the file was renamed away (likely out of the
    /// workspace) — that's a removal from our perspective. A stranded
    /// destination means the file appeared (likely renamed in from outside)
    /// — semantically a modification of that path.
    fn flush_orphan(p: PendingRename, editor_tx: &UnboundedSender<FileWatcherEvent>) {
        let event = if p.is_source {
            FileWatcherEvent::Removed(p.helix_path)
        } else {
            FileWatcherEvent::Modified(p.helix_path)
        };
        let _ = editor_tx.send(event);
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;

        /// `tempfile::tempdir()` lives under `/var/folders/...` on macOS,
        /// which is itself a symlink to `/private/var/folders/...`. The
        /// ignore matchers are built against the canonical (symlink-
        /// resolved) form, so tests must compare against that too.
        fn canonical(dir: &tempfile::TempDir) -> PathBuf {
            std::fs::canonicalize(dir.path()).unwrap()
        }

        #[test]
        fn root_pattern_ignores_matching_file() {
            let tmp = tempfile::tempdir().unwrap();
            fs::write(tmp.path().join(".gitignore"), "*.tmp\n").unwrap();
            let root = canonical(&tmp);
            let ignore = build_ignore(&root);

            assert!(ignore.matches(&root.join("foo.tmp")));
            assert!(!ignore.matches(&root.join("foo.rs")));
        }

        #[test]
        fn nested_pattern_scopes_to_subdirectory() {
            let tmp = tempfile::tempdir().unwrap();
            let root = canonical(&tmp);
            fs::create_dir(root.join("sub")).unwrap();
            fs::write(root.join("sub/.gitignore"), "secret\n").unwrap();
            let ignore = build_ignore(&root);

            // Pattern is scoped to /sub: matches /sub/secret, not /secret.
            assert!(ignore.matches(&root.join("sub/secret")));
            assert!(!ignore.matches(&root.join("secret")));
        }

        #[test]
        fn deeper_whitelist_overrides_shallower_ignore() {
            let tmp = tempfile::tempdir().unwrap();
            let root = canonical(&tmp);
            fs::write(root.join(".gitignore"), "*.log\n").unwrap();
            fs::create_dir(root.join("keep")).unwrap();
            fs::write(root.join("keep/.gitignore"), "!important.log\n").unwrap();
            let ignore = build_ignore(&root);

            assert!(ignore.matches(&root.join("foo.log")));
            assert!(ignore.matches(&root.join("keep/other.log")));
            assert!(!ignore.matches(&root.join("keep/important.log")));
        }

        #[test]
        fn git_directory_is_always_ignored() {
            let tmp = tempfile::tempdir().unwrap();
            let root = canonical(&tmp);
            // Don't even create .git on disk — the unconditional rule
            // shouldn't depend on filesystem state.
            let ignore = build_ignore(&root);

            assert!(ignore.matches(&root.join(".git/HEAD")));
            assert!(ignore.matches(&root.join(".git/objects/pack")));
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
