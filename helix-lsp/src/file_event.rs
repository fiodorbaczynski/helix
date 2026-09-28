use std::{collections::HashMap, path::PathBuf, sync::Weak};

use globset::{GlobBuilder, GlobSetBuilder};
use tokio::sync::mpsc;

use crate::{lsp, Client, LanguageServerId};

enum Event {
    FileChanged {
        path: PathBuf,
    },
    Register {
        client_id: LanguageServerId,
        client: Weak<Client>,
        registration_id: String,
        options: lsp::DidChangeWatchedFilesRegistrationOptions,
    },
    Unregister {
        client_id: LanguageServerId,
        registration_id: String,
    },
    RemoveClient {
        client_id: LanguageServerId,
    },
}

#[derive(Default)]
struct ClientState {
    client: Weak<Client>,
    registered: HashMap<String, globset::GlobSet>,
}

/// The Handler uses a dedicated tokio task to respond to file change events by
/// forwarding changes to LSPs that have registered for notifications with a
/// matching glob.
///
/// When an LSP registers for the DidChangeWatchedFiles notification, the
/// Handler is notified by sending the registration details in addition to a
/// weak reference to the LSP client. This is done so that the Handler can have
/// access to the client without preventing the client from being dropped if it
/// is closed and the Handler isn't properly notified.
#[derive(Clone, Debug)]
pub struct Handler {
    tx: mpsc::UnboundedSender<Event>,
}

impl Default for Handler {
    fn default() -> Self {
        Self::new()
    }
}

impl Handler {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Self::run(rx));
        Self { tx }
    }

    pub fn register(
        &self,
        client_id: LanguageServerId,
        client: Weak<Client>,
        registration_id: String,
        options: lsp::DidChangeWatchedFilesRegistrationOptions,
    ) {
        let _ = self.tx.send(Event::Register {
            client_id,
            client,
            registration_id,
            options,
        });
    }

    pub fn unregister(&self, client_id: LanguageServerId, registration_id: String) {
        let _ = self.tx.send(Event::Unregister {
            client_id,
            registration_id,
        });
    }

    pub fn file_changed(&self, path: PathBuf) {
        let _ = self.tx.send(Event::FileChanged { path });
    }

    pub fn remove_client(&self, client_id: LanguageServerId) {
        let _ = self.tx.send(Event::RemoveClient { client_id });
    }

    async fn run(mut rx: mpsc::UnboundedReceiver<Event>) {
        let mut state: HashMap<LanguageServerId, ClientState> = HashMap::new();
        while let Some(event) = rx.recv().await {
            match event {
                Event::FileChanged { path } => {
                    log::debug!("Received file event for {:?}", &path);

                    state.retain(|id, client_state| {
                        if !client_state
                            .registered
                            .values()
                            .any(|glob| glob.is_match(&path))
                        {
                            return true;
                        }
                        let Some(client) = client_state.client.upgrade() else {
                            log::warn!("LSP client was dropped: {id}");
                            return false;
                        };
                        let Ok(uri) = lsp::Url::from_file_path(&path) else {
                            return true;
                        };
                        log::debug!(
                            "Sending didChangeWatchedFiles notification to client '{}'",
                            client.name()
                        );
                        client.did_change_watched_files(vec![lsp::FileEvent {
                            uri,
                            // We currently always send the CHANGED state
                            // since we don't actually have more context at
                            // the moment.
                            typ: lsp::FileChangeType::CHANGED,
                        }]);
                        true
                    });
                }
                Event::Register {
                    client_id,
                    client,
                    registration_id,
                    options: ops,
                } => {
                    log::debug!(
                        "Registering didChangeWatchedFiles for client '{}' with id '{}'",
                        client_id,
                        registration_id
                    );

                    let entry = state.entry(client_id).or_default();
                    entry.client = client;

                    let mut builder = GlobSetBuilder::new();
                    for watcher in ops.watchers {
                        let Some(pattern) = resolve_glob_pattern(watcher.glob_pattern) else {
                            continue;
                        };
                        match GlobBuilder::new(&pattern).build() {
                            Ok(glob) => {
                                builder.add(glob);
                            }
                            Err(err) => log::warn!(
                                "ignoring didChangeWatchedFiles pattern `{pattern}`: {err}"
                            ),
                        }
                    }
                    match builder.build() {
                        Ok(globset) => {
                            entry.registered.insert(registration_id, globset);
                        }
                        Err(err) => {
                            // Remove any old state for that registration id and
                            // remove the entire client if it's now empty.
                            entry.registered.remove(&registration_id);
                            if entry.registered.is_empty() {
                                state.remove(&client_id);
                            }
                            log::warn!(
                                "Unable to build globset for LSP didChangeWatchedFiles {err}"
                            )
                        }
                    }
                }
                Event::Unregister {
                    client_id,
                    registration_id,
                } => {
                    log::debug!(
                        "Unregistering didChangeWatchedFiles with id '{}' for client '{}'",
                        registration_id,
                        client_id
                    );
                    if let Some(client_state) = state.get_mut(&client_id) {
                        client_state.registered.remove(&registration_id);
                        if client_state.registered.is_empty() {
                            state.remove(&client_id);
                        }
                    }
                }
                Event::RemoveClient { client_id } => {
                    log::debug!("Removing LSP client: {client_id}");
                    state.remove(&client_id);
                }
            }
        }
    }
}

/// Reduce an LSP `GlobPattern` to a single glob string suitable for
/// `globset::GlobBuilder`.
///
/// LSP 3.17 introduced `RelativePattern { baseUri, pattern }` as an
/// alternative to plain string patterns; servers prefer it when the client
/// advertises `relativePatternSupport`. We resolve the base URI to a path
/// and prepend it to the pattern so the resulting glob can match against
/// absolute filesystem paths.
///
/// Returns `None` if the base URI of a relative pattern isn't a `file:` URI
/// — there is no meaningful filesystem glob in that case.
fn resolve_glob_pattern(pattern: lsp::GlobPattern) -> Option<String> {
    match pattern {
        lsp::GlobPattern::String(pattern) => Some(pattern),
        lsp::GlobPattern::Relative(relative) => {
            let base_uri = match relative.base_uri {
                lsp::OneOf::Left(folder) => folder.uri,
                lsp::OneOf::Right(uri) => uri,
            };
            let base = match base_uri.to_file_path() {
                Ok(path) => path,
                Err(()) => {
                    log::warn!(
                        "ignoring didChangeWatchedFiles relative pattern with non-file baseUri: {base_uri}"
                    );
                    return None;
                }
            };
            // `globset` parses patterns with `/` as the path separator on
            // every platform; normalise so Windows base paths compose too.
            let base = base.to_string_lossy().replace('\\', "/");
            Some(format!(
                "{}/{}",
                base.trim_end_matches('/'),
                relative.pattern
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relative(base: &str, pattern: &str) -> lsp::GlobPattern {
        lsp::GlobPattern::Relative(lsp::RelativePattern {
            base_uri: lsp::OneOf::Right(lsp::Url::parse(base).unwrap()),
            pattern: pattern.to_owned(),
        })
    }

    #[test]
    fn string_pattern_passes_through() {
        let pattern = lsp::GlobPattern::String("**/*.rs".to_owned());
        assert_eq!(resolve_glob_pattern(pattern).as_deref(), Some("**/*.rs"));
    }

    #[test]
    fn relative_pattern_prepends_base_path() {
        let pattern = relative("file:///workspace/crate", "**/*.rs");
        assert_eq!(
            resolve_glob_pattern(pattern).as_deref(),
            Some("/workspace/crate/**/*.rs"),
        );
    }

    #[test]
    fn relative_pattern_strips_trailing_slash_from_base() {
        // Some servers terminate the base URI with `/`. Composing naively
        // would produce `//**/*.rs`, which globset treats as a different
        // pattern than `/**/*.rs`.
        let pattern = relative("file:///workspace/crate/", "**/*.rs");
        assert_eq!(
            resolve_glob_pattern(pattern).as_deref(),
            Some("/workspace/crate/**/*.rs"),
        );
    }

    #[test]
    fn relative_pattern_resolved_glob_matches_descendant() {
        // End-to-end: the resolved string, fed back through globset, must
        // match an absolute path inside the base — this is the contract the
        // call site relies on.
        let pattern = relative("file:///workspace/crate", "**/*.rs");
        let resolved = resolve_glob_pattern(pattern).unwrap();
        let glob = GlobBuilder::new(&resolved).build().unwrap().compile_matcher();
        assert!(glob.is_match("/workspace/crate/src/lib.rs"));
        assert!(glob.is_match("/workspace/crate/src/nested/mod.rs"));
        assert!(!glob.is_match("/workspace/other/src/lib.rs"));
    }

    #[test]
    fn relative_pattern_with_workspace_folder_base() {
        let pattern = lsp::GlobPattern::Relative(lsp::RelativePattern {
            base_uri: lsp::OneOf::Left(lsp::WorkspaceFolder {
                uri: lsp::Url::parse("file:///workspace/crate").unwrap(),
                name: "crate".to_owned(),
            }),
            pattern: "**/Cargo.toml".to_owned(),
        });
        assert_eq!(
            resolve_glob_pattern(pattern).as_deref(),
            Some("/workspace/crate/**/Cargo.toml"),
        );
    }

    #[test]
    fn non_file_base_uri_is_skipped() {
        let pattern = relative("https://example.com/", "**/*.rs");
        assert_eq!(resolve_glob_pattern(pattern), None);
    }
}
