//! External-change orchestration for the workspace file watcher.
//!
//! The watcher (in `helix_view::handlers::file_watcher`) emits
//! `FileWatcherEvent`s on the editor's main loop. This module decides what
//! to do with each event: which buffers to reload, when to skip because of
//! unsaved changes, and what to surface in the status line. The reload
//! mechanics themselves live on `Editor::reload_document`.

use std::path::PathBuf;

use helix_view::handlers::file_watcher::FileWatcherEvent;
use helix_view::{DocumentId, Editor};

/// Handle one external filesystem event.
pub fn handle(event: FileWatcherEvent, editor: &mut Editor) {
    if !editor.config().auto_reload {
        return;
    }

    match event {
        FileWatcherEvent::Modified(path) => handle_modified(path, editor),
        FileWatcherEvent::Removed(path) => handle_removed(path, editor),
        FileWatcherEvent::Renamed { from, to } => handle_renamed(from, to, editor),
    }
}

fn handle_modified(path: PathBuf, editor: &mut Editor) {
    let Some((doc_id, path)) = lookup(path, editor) else {
        return;
    };

    // FSEvents replays historical events on workspace open. A Modified for
    // a path that no longer exists is a stale replay — don't try to act on
    // it, and don't clear `deleted_on_disk` for a file that's still gone.
    if !path.exists() {
        return;
    }

    let doc = doc_mut!(editor, &doc_id);
    if doc.is_modified() {
        // Buffer is dirty and disk content has changed under us. Flag the
        // divergence; the user resolves via `:reload` (discard buffer) or
        // `:write` (overwrite disk). Clearing `deleted_on_disk` covers
        // delete → recreate round-trips that left the flag set.
        doc.deleted_on_disk = false;
        doc.disk_changed_unresolved = true;
        log::info!("disk diverged from dirty buffer: {}", path.display());
        return;
    }

    let name = doc!(editor, &doc_id).display_name().into_owned();
    match editor.reload_document(doc_id) {
        Ok(()) => log::info!("auto-reloaded {}", path.display()),
        Err(err) => editor.set_error(format!("{name}: {err}")),
    }
}

fn handle_removed(path: PathBuf, editor: &mut Editor) {
    let Some((doc_id, path)) = lookup(path, editor) else {
        return;
    };

    let doc = doc_mut!(editor, &doc_id);
    if doc.is_modified() {
        // Dirty buffer: keep its contents in memory, flag it so the
        // statusline can warn the user their disk state is gone.
        doc.deleted_on_disk = true;
        log::info!("external delete with unsaved changes: {}", path.display());
        return;
    }

    // Clean buffer: nothing to lose, close it. The two error cases —
    // `DoesNotExist` and `BufferModified` — are unreachable here because we
    // just looked the doc up and confirmed it isn't dirty.
    if editor.close_document(doc_id, false).is_err() {
        unreachable!("close_document on freshly-checked clean buffer");
    }
    log::info!("auto-closed externally removed {}", path.display());
}

fn handle_renamed(from: PathBuf, to: PathBuf, editor: &mut Editor) {
    let from = helix_stdx::path::canonicalize(&from);
    let to = helix_stdx::path::canonicalize(&to);
    let Some(doc_id) = editor.document_id_by_path(&from) else {
        return;
    };
    // Always follow regardless of dirty state — the user's edit is still
    // valid, just at a new location. Save would target the new path next.
    editor.set_doc_path(doc_id, &to);
    log::info!("auto-followed rename {} -> {}", from.display(), to.display());
}

/// Resolve an event path to a `(DocumentId, canonical_path)` if a buffer
/// is currently open for it. The canonical path is returned for logging.
fn lookup(path: PathBuf, editor: &Editor) -> Option<(DocumentId, PathBuf)> {
    let path = helix_stdx::path::canonicalize(&path);
    editor.document_id_by_path(&path).map(|id| (id, path))
}
