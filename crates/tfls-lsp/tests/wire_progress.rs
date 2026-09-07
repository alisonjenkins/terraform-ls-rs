//! `window/workDoneProgress/create` must only be sent to clients that
//! advertise `window.workDoneProgress`. A client without it never answers
//! the request, and the single serial indexer worker blocks on it forever.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use serde_json::json;
use std::fs;
use std::path::PathBuf;
use support::TestClient;

const PROGRESS_CREATE: &str = "window/workDoneProgress/create";

/// A module dir with an (empty) `.terraform/providers/` so `did_open`
/// enqueues a schema fetch, which reports progress. Enqueued after
/// `initialize` has completed, so the request is never short-circuited
/// by tower-lsp's not-yet-initialized guard.
fn workspace_with_providers_dir(tag: &str) -> (PathBuf, String) {
    let workspace = std::env::temp_dir().join(format!(
        "tfls-wire-progress-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(workspace.join(".terraform/providers")).unwrap();
    let main_tf = workspace.join("main.tf");
    let text = "variable \"a\" {}\n";
    fs::write(&main_tf, text).unwrap();
    let uri = url::Url::from_file_path(&main_tf).unwrap().to_string();
    (workspace, uri)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_progress_create_without_client_capability() {
    let (workspace, uri) = workspace_with_providers_dir("nocap");

    let mut client = TestClient::new();
    client.initialize(None).await;
    client.did_open(&uri, "variable \"a\" {}\n").await;
    client.settle(1500).await;

    assert_eq!(
        client.count_method(PROGRESS_CREATE).await,
        0,
        "server sent {PROGRESS_CREATE} to a client that did not advertise window.workDoneProgress",
    );

    client.shutdown().await;
    fs::remove_dir_all(&workspace).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_create_when_client_advertises_capability() {
    let (workspace, uri) = workspace_with_providers_dir("cap");

    let mut client = TestClient::new();
    client
        .initialize_with_capabilities(
            None,
            json!({
                "textDocument": {},
                "workspace": {},
                "window": { "workDoneProgress": true }
            }),
        )
        .await;
    client.did_open(&uri, "variable \"a\" {}\n").await;
    client.settle(1500).await;

    assert!(
        client.count_method(PROGRESS_CREATE).await >= 1,
        "expected at least one {PROGRESS_CREATE} for a progress-capable client",
    );

    client.shutdown().await;
    fs::remove_dir_all(&workspace).ok();
}
