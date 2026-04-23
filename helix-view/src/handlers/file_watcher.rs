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
    use super::PathBuf;

    pub struct FileWatcher {
        _root: PathBuf,
    }

    impl FileWatcher {
        pub fn start(root: PathBuf) -> Option<Self> {
            Some(Self { _root: root })
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
