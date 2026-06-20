//! Workspace-level notifications: config changes and client-driven
//! file watches.

use lsp_types::{DidChangeConfigurationParams, DidChangeWatchedFilesParams, FileChangeType};
use tfls_state::{Job, Priority};

use crate::backend::Backend;

pub async fn did_change_configuration(backend: &Backend, params: DidChangeConfigurationParams) {
    // tower-lsp gives us `serde_json::Value`; route through sonic-rs
    // for consistency with the rest of the server.
    let Ok(json) = serde_json::to_string(&params.settings) else {
        tracing::warn!("didChangeConfiguration: failed to serialise settings");
        return;
    };
    let Ok(sonic) = sonic_rs::from_str::<sonic_rs::Value>(&json) else {
        tracing::warn!("didChangeConfiguration: failed to reparse settings");
        return;
    };
    backend.state.config.update_from_json(&sonic);
    tracing::info!("applied didChangeConfiguration");

    // Config can change which diagnostics fire (e.g. the `styleRules`
    // toggle, formatStyle). Recompute + republish open buffers so the
    // change is live — otherwise the toggle silently no-ops until the
    // user edits each file, and stale diagnostics linger after toggling
    // a rule off.
    crate::indexer::republish_open_docs(&backend.state, &backend.client).await;
}

pub async fn did_change_watched_files(backend: &Backend, params: DidChangeWatchedFilesParams) {
    for event in params.changes {
        let Some(file_url) = tfls_core::uri::uri_to_url(&event.uri) else {
            tracing::warn!(uri = event.uri.as_str(), "watched file URI is not a path");
            continue;
        };
        let Ok(path) = file_url.to_file_path() else {
            tracing::warn!(uri = event.uri.as_str(), "watched file URI is not a path");
            continue;
        };
        match event.typ {
            FileChangeType::CREATED | FileChangeType::CHANGED => {
                backend.jobs.enqueue(Job::ParseFile(path), Priority::Normal);
            }
            FileChangeType::DELETED => {
                // Don't drop an OPEN buffer: the editor still has it (a
                // disk-side delete is often a transient save/rename or a
                // `git checkout` of a branch lacking the file). Removing it
                // would desync — the editor keeps editing a doc the server
                // no longer knows about. The editor's own did_close is the
                // authoritative signal to remove an open doc.
                if backend.state.is_open(&file_url) {
                    continue;
                }
                backend.state.remove_document(&file_url);
                // Clear the deleted file's published diagnostics (they'd
                // otherwise linger in the client forever), then refresh
                // open peers in the same dir — deleting e.g. variables.tf
                // invalidates sibling reference resolution.
                backend
                    .client
                    .publish_diagnostics(event.uri.clone(), Vec::new(), None)
                    .await;
                crate::handlers::document::publish_peer_diagnostics(backend, &file_url).await;
            }
            _ => {}
        }
    }
}
