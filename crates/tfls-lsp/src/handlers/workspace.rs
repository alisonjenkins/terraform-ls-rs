//! Workspace-level notifications: config changes and client-driven
//! file watches.

use lsp_types::{
    DidChangeConfigurationParams, DidChangeWatchedFilesParams, FileChangeType,
};
use tfls_state::{Job, Priority};

use crate::backend::Backend;

pub async fn did_change_configuration(
    backend: &Backend,
    params: DidChangeConfigurationParams,
) {
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
}

pub async fn did_change_watched_files(backend: &Backend, params: DidChangeWatchedFilesParams) {
    for event in params.changes {
        let Ok(path) = event.uri.to_file_path() else {
            tracing::warn!(uri = %event.uri, "watched file URI is not a path");
            continue;
        };
        match event.typ {
            FileChangeType::CREATED | FileChangeType::CHANGED => {
                backend
                    .jobs
                    .enqueue(Job::ParseFile(path), Priority::Normal);
            }
            FileChangeType::DELETED => {
                backend.state.remove_document(&event.uri);
            }
            _ => {}
        }
    }
}
