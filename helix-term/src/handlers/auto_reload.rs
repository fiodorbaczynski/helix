//! External-change orchestration for the workspace file watcher.
//!
//! The watcher (in `helix_view::handlers::file_watcher`) emits
//! `FileWatcherEvent`s on the editor's main loop. This module decides what
//! to do with each event: which buffers to reload, when to skip because of
//! unsaved changes, and what to surface in the status line. The reload
//! mechanics themselves live on `Editor::reload_document`.

use helix_view::handlers::file_watcher::FileWatcherEvent;
use helix_view::Editor;

/// Handle one external filesystem event.
pub fn handle(event: FileWatcherEvent, editor: &mut Editor) {
    let FileWatcherEvent::Modified(path) = event;

    if !editor.config().auto_reload {
        return;
    }

    let path = helix_stdx::path::canonicalize(&path);
    let Some(doc_id) = editor.document_id_by_path(&path) else {
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
