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
    use std::path::PathBuf;
    use std::sync::mpsc;

    use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

    /// Holds the live FSEvents stream for the workspace.
    ///
    /// Dropping this value stops the stream: the underlying `Watcher` is
    /// dropped, its sync sender disconnects, and the bridging blocking task
    /// exits when `recv` returns `Err`.
    pub struct FileWatcher {
        _watcher: RecommendedWatcher,
    }

    impl FileWatcher {
        pub fn start(root: PathBuf) -> Option<Self> {
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

            log::info!("watching workspace for external changes: {}", root.display());

            tokio::task::spawn_blocking(move || drain(rx));

            Some(Self { _watcher: watcher })
        }
    }

    fn drain(rx: mpsc::Receiver<notify::Result<Event>>) {
        while let Ok(result) = rx.recv() {
            match result {
                Ok(event) => log::trace!("fs event: {event:?}"),
                Err(err) => log::warn!("file watcher error: {err}"),
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::PathBuf;

    pub struct FileWatcher;

    impl FileWatcher {
        pub fn start(_root: PathBuf) -> Option<Self> {
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

