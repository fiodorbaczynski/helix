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
    }
}

fn handle_modified(path: PathBuf, editor: &mut Editor) {
    let Some((doc_id, path)) = lookup(path, editor) else {
        return;
    };

    let doc = doc!(editor, &doc_id);
    if doc.is_modified() {
        let name = doc.display_name().into_owned();
        editor.set_status(format!(
            "{name} changed on disk; buffer has unsaved changes (use :reload to overwrite)"
        ));
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

/// Resolve an event path to a `(DocumentId, canonical_path)` if a buffer
/// is currently open for it. The canonical path is returned for logging.
fn lookup(path: PathBuf, editor: &Editor) -> Option<(DocumentId, PathBuf)> {
    let path = helix_stdx::path::canonicalize(&path);
    editor.document_id_by_path(&path).map(|id| (id, path))
}
