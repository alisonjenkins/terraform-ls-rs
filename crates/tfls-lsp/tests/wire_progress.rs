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

/// A client that advertises `window.workDoneProgress` but — like
/// `TestClient` here — never answers `window/workDoneProgress/create`
/// is exactly the misbehaving-client case: real editors always
/// respond, but a slow or buggy one might not. `ProgressReporter::begin`
/// awaits that response, and the indexer is a single serial worker, so
/// an unanswered create request wedges every later job forever —
/// including the `rebuild_assigned_variable_types` scan that this
/// workspace's `variables.tf` needs before the tfvars-based quick-fix
/// can appear. Before the bounded-wait fix, this test times out; after
/// it, the wedged progress request degrades to "no progress reporting"
/// instead of "no indexing".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexing_survives_unanswered_progress_create() {
    use std::fs;

    let workspace = std::env::temp_dir().join(format!(
        "tfls-wire-progress-wedge-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(workspace.join("params/nonprod")).unwrap();

    let main_tf = workspace.join("variables.tf");
    let main_text = "variable \"envtype\" {}\noutput \"e\" { value = var.envtype }\n";
    fs::write(&main_tf, main_text).unwrap();
    fs::write(
        workspace.join("params/nonprod/params.tfvars"),
        "envtype = \"nonprod\"\n",
    )
    .unwrap();

    let workspace_uri = url::Url::from_file_path(&workspace).unwrap().to_string();
    let main_uri = url::Url::from_file_path(&main_tf).unwrap().to_string();

    let mut client = TestClient::new();
    client
        .initialize_with_capabilities(
            Some(&workspace_uri),
            json!({
                "textDocument": {},
                "workspace": {},
                "window": { "workDoneProgress": true }
            }),
        )
        .await;

    // `initialize` unconditionally enqueues `Job::FetchSchemas` /
    // `Job::FetchFunctions` / `Job::BulkWorkspaceScan` for the workspace
    // root — each opens a progress span before doing real work. Give the
    // single serial worker time to dequeue one of them and reach the
    // `window/workDoneProgress/create` await (state is already
    // `Initialized` well before this settle returns, so the request is
    // genuinely sent rather than short-circuited by tower-lsp's
    // not-yet-initialized guard). This makes the wedge deterministic:
    // by the time `did_open` enqueues its own `Job::ScanDirectory`, the
    // worker is already stuck (pre-fix) behind the unanswered request.
    client.settle(300).await;
    client.did_open(&main_uri, main_text).await;

    // Budget comfortably exceeds `progress::CLIENT_REQUEST_TIMEOUT`
    // (5s) so a fixed timeout degradation still leaves plenty of room
    // for the subsequent indexing work to complete.
    let wanted = |a: &serde_json::Value| {
        let t = a["title"].as_str().unwrap_or("");
        t.contains("Set variable type to `string`") && t.contains("tfvars / module callers")
    };
    let mut resp = json!(null);
    let mut found = false;
    for _ in 0..60 {
        client.settle(250).await;
        resp = client
            .code_action(
                &main_uri,
                json!({
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 22 }
                }),
                json!([{
                    "range": {
                        "start": { "line": 0, "character": 9 },
                        "end": { "line": 0, "character": 17 }
                    },
                    "severity": 2,
                    "source": "terraform-ls-rs",
                    "message": "`envtype` variable has no type"
                }]),
            )
            .await;
        let actions = resp["result"].as_array().cloned().unwrap_or_default();
        if actions.iter().any(wanted) {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "expected a `Set variable type to `string` … tfvars / module callers` action even with an unanswered {PROGRESS_CREATE}; got {resp}",
    );
    assert!(
        client.count_method(PROGRESS_CREATE).await >= 1,
        "expected the server to have attempted at least one {PROGRESS_CREATE}",
    );

    client.shutdown().await;
    fs::remove_dir_all(&workspace).ok();
}
