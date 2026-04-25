//! Wire-level regression tests for cross-file diagnostic refresh.
//!
//! These pin the server-to-client `textDocument/publishDiagnostics`
//! stream — the protocol surface that real clients (Neovim, VS Code,
//! Helix, Trouble) actually consume. Server-side state correctness is
//! covered by `diagnostics.rs`; this file covers the wire half so a
//! regression that breaks notification delivery (a la "we recompute
//! correctly but never push to peers") is caught in CI.
//!
//! Strategy:
//!
//! 1. Spin up a real `LspService<Backend>` with the same `LspService`
//!    machinery the production stdio binary uses.
//! 2. Capture every server-initiated message from the loopback socket
//!    into a shared `Vec`.
//! 3. Drive `initialize` → `initialized` → `didOpen` x2 → `didChange`
//!    via `tower::Service::call`.
//! 4. After the edit, inspect the captured stream and assert that the
//!    LAST `publishDiagnostics` for the peer file reflects the
//!    post-fix state.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use tfls_lsp::Backend;
use tokio::sync::Mutex;
use tower::Service;
use tower_lsp::LspService;
use tower_lsp::jsonrpc::Request;

type Captured = Arc<Mutex<Vec<Value>>>;

/// Build a service plus a background drainer that records every
/// server→client message as a JSON `Value`. Returns the service, the
/// shared capture handle, and the drainer's join handle so the caller
/// can dispose of it cleanly.
fn make_capturing_service() -> (
    LspService<Backend>,
    Captured,
    tokio::task::JoinHandle<()>,
) {
    let (service, socket) = LspService::new(Backend::new);
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = captured.clone();
    let drainer = tokio::spawn(async move {
        let mut socket = socket;
        while let Some(req) = socket.next().await {
            if let Ok(json) = serde_json::to_value(&req) {
                captured_clone.lock().await.push(json);
            }
        }
    });
    (service, captured, drainer)
}

async fn call(service: &mut LspService<Backend>, req: Request) -> Value {
    let resp = service
        .call(req)
        .await
        .expect("service")
        .expect("response present");
    serde_json::to_value(&resp).expect("response json")
}

async fn notify(service: &mut LspService<Backend>, req: Request) {
    let _ = service.call(req).await.expect("notify");
}

/// Pull every `textDocument/publishDiagnostics` notification from the
/// captured stream whose params.uri matches `target`. Returns the
/// `diagnostics` array on each — newest last.
async fn diagnostics_for(captured: &Captured, target: &str) -> Vec<Vec<Value>> {
    let lock = captured.lock().await;
    lock.iter()
        .filter_map(|msg| {
            // Server→client notifications come through the socket as
            // `jsonrpc::Request` payloads with `id == None`. Only
            // those without an id and a matching method are publishes.
            let method = msg.get("method")?.as_str()?;
            if method != "textDocument/publishDiagnostics" {
                return None;
            }
            let params = msg.get("params")?;
            if params.get("uri")?.as_str()? != target {
                return None;
            }
            params.get("diagnostics")?.as_array().cloned()
        })
        .collect()
}

async fn last_diagnostics_for(captured: &Captured, target: &str) -> Vec<Value> {
    diagnostics_for(captured, target)
        .await
        .pop()
        .unwrap_or_default()
}

fn contains_undefined_var(diags: &[Value], name: &str) -> bool {
    diags.iter().any(|d| {
        d.get("message")
            .and_then(|m| m.as_str())
            .map(|m| m.contains("undefined variable") && m.contains(name))
            .unwrap_or(false)
    })
}

#[tokio::test]
async fn peer_file_undefined_variable_clears_after_declaration_added() {
    // The user's reported bug, captured at the wire level: edit
    // variables.tf to add `variable "foo" {}`. main.tf's wire-level
    // publishDiagnostics for `var.foo` must drop on the next push.

    let (mut service, captured, drainer) = make_capturing_service();

    let _ = call(
        &mut service,
        Request::build("initialize")
            .id(1)
            .params(json!({
                "processId": null,
                "rootUri": null,
                "capabilities": {
                    "textDocument": {},
                    "workspace": {}
                }
            }))
            .finish(),
    )
    .await;
    notify(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    let main_uri = "file:///mod/main.tf";
    let vars_uri = "file:///mod/variables.tf";

    // Open main.tf with an undefined `var.foo` reference.
    notify(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": main_uri,
                    "languageId": "terraform",
                    "version": 1,
                    "text": "output \"x\" { value = var.foo }\n"
                }
            }))
            .finish(),
    )
    .await;

    // Open variables.tf empty (no `foo` declaration yet).
    notify(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": vars_uri,
                    "languageId": "terraform",
                    "version": 1,
                    "text": ""
                }
            }))
            .finish(),
    )
    .await;

    // Yield so async publishes are flushed before we assert the
    // baseline. The handlers spawn blocking work for compute_diagnostics
    // and the publish goes through tower-lsp's outgoing channel.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let baseline = last_diagnostics_for(&captured, main_uri).await;
    assert!(
        contains_undefined_var(&baseline, "foo"),
        "baseline: expected undefined-var on main.tf for `foo`, got {baseline:?}"
    );

    // The fix: user types the declaration into variables.tf. Send a
    // full-document replacement (range: null) — same shape nvim emits
    // when typing into a previously-empty buffer.
    notify(
        &mut service,
        Request::build("textDocument/didChange")
            .params(json!({
                "textDocument": { "uri": vars_uri, "version": 2 },
                "contentChanges": [
                    { "text": "variable \"foo\" {}\n" }
                ]
            }))
            .finish(),
    )
    .await;

    // Wait for did_change → reparse → publish_peer_diagnostics to
    // complete. The test fails if 250ms isn't enough; bump it on slow
    // CI rather than racing.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let pushes = diagnostics_for(&captured, main_uri).await;
    let final_diags = pushes.last().cloned().unwrap_or_default();
    assert!(
        !contains_undefined_var(&final_diags, "foo"),
        "after peer-file fix: undefined-var on main.tf must clear; \
         all pushes for main.tf were: {pushes:?}"
    );

    drop(service);
    let _ = tokio::time::timeout(Duration::from_secs(1), drainer).await;
}

#[tokio::test]
async fn server_does_not_advertise_pull_diagnostics() {
    // Pinning the capability decision: nvim 0.11+ keeps push and pull
    // in SEPARATE namespaces and renders the union. Advertising
    // `diagnosticProvider` made nvim auto-pull on every didOpen,
    // populating the pull namespace with a one-shot snapshot that
    // never refreshed when a peer file was edited — push pushed
    // fresh data but the union still showed the stale pull entries.
    // Push is now the only channel; this test fails loudly if a
    // future commit re-advertises pull without rethinking the
    // dual-namespace problem.

    let (mut service, _captured, drainer) = make_capturing_service();
    let body = call(
        &mut service,
        Request::build("initialize")
            .id(1)
            .params(json!({
                "processId": null,
                "rootUri": null,
                "capabilities": {}
            }))
            .finish(),
    )
    .await;

    let caps = &body["result"]["capabilities"];
    assert!(
        caps.get("diagnosticProvider").is_none() || caps["diagnosticProvider"].is_null(),
        "`diagnosticProvider` must be absent — nvim's dual-namespace \
         render bug is the reason. caps were: {caps}"
    );

    drop(service);
    let _ = tokio::time::timeout(Duration::from_secs(1), drainer).await;
}

/// User-reported scenario: root module declares `variable "envtype" {}`
/// (no type, no default); env-split tfvars in `params/{nonprod,prod}/`
/// each assign `envtype = "..."`. After `didOpen`, a `codeAction`
/// request at the variable's diagnostic range must return the
/// `Set variable type to \`string\`` quick-fix.
///
/// This reproduces the wire behaviour the user couldn't get out of
/// nvim. A passing test means the server is correct end-to-end —
/// any "no action shown in editor" is then client-side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_action_envtype_inference_via_wire() {
    use std::fs;

    let workspace = std::env::temp_dir().join(format!(
        "tfls-wire-envtype-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(workspace.join("params/nonprod")).unwrap();
    fs::create_dir_all(workspace.join("params/prod")).unwrap();

    let main_tf = workspace.join("variables.tf");
    let main_text = "variable \"envtype\" {}\noutput \"e\" { value = var.envtype }\n";
    fs::write(&main_tf, main_text).unwrap();
    fs::write(
        workspace.join("params/nonprod/params.tfvars"),
        "envtype = \"nonprod\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("params/prod/params.tfvars"),
        "envtype = \"prod\"\n",
    )
    .unwrap();

    let workspace_uri = lsp_types::Url::from_file_path(&workspace).unwrap().to_string();
    let main_uri = lsp_types::Url::from_file_path(&main_tf).unwrap().to_string();

    let (mut service, _captured, drainer) = make_capturing_service();

    let _ = call(
        &mut service,
        Request::build("initialize")
            .id(1)
            .params(json!({
                "processId": null,
                "rootUri": workspace_uri,
                "capabilities": {}
            }))
            .finish(),
    )
    .await;
    notify(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    notify(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": main_uri,
                    "languageId": "terraform",
                    "version": 1,
                    "text": main_text,
                }
            }))
            .finish(),
    )
    .await;

    // ScanDirectory + rebuild fire on the worker; give them a
    // beat to populate `assigned_variable_types`.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let resp = call(
        &mut service,
        Request::build("textDocument/codeAction")
            .id(2)
            .params(json!({
                "textDocument": { "uri": main_uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 22 }
                },
                "context": {
                    "diagnostics": [{
                        "range": {
                            "start": { "line": 0, "character": 9 },
                            "end": { "line": 0, "character": 17 }
                        },
                        "severity": 2,
                        "source": "terraform-ls-rs",
                        "message": "`envtype` variable has no type"
                    }]
                }
            }))
            .finish(),
    )
    .await;

    let actions = resp["result"].as_array().cloned().unwrap_or_default();
    assert!(
        !actions.is_empty(),
        "expected the `Set variable type` quick-fix; got {resp}",
    );
    let title = actions[0]["title"].as_str().unwrap_or("");
    assert!(
        title.contains("Set variable type to `string`")
            && title.contains("tfvars / module callers"),
        "unexpected action title: {title}",
    );

    drop(service);
    let _ = tokio::time::timeout(Duration::from_secs(1), drainer).await;
    fs::remove_dir_all(&workspace).ok();
}
