//! Full end-to-end LSP protocol test driving [`LspService`] with real
//! JSON-RPC messages. Validates the initialize → didOpen → completion
//! → shutdown flow.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use futures::StreamExt;
use serde_json::json;
use tfls_lsp::Backend;
use tower::Service;
use tower_lsp::LspService;
use tower_lsp::jsonrpc::Request;

/// Drive the service through a full lifecycle and verify each reply.
/// We spawn a drainer on the server→client socket so that
/// `publish_diagnostics` and similar server-initiated messages don't
/// block the handlers (the socket is a bounded mpsc).
async fn make_service() -> (
    LspService<Backend>,
    tokio::task::JoinHandle<()>,
) {
    let (service, socket) = LspService::new(Backend::new);
    // Drain server→client notifications in the background.
    let drainer = tokio::spawn(async move {
        let mut socket = socket;
        while socket.next().await.is_some() {}
    });
    (service, drainer)
}

async fn call_json(
    service: &mut LspService<Backend>,
    req: Request,
) -> serde_json::Value {
    let resp = service
        .call(req)
        .await
        .expect("service ok")
        .expect("response present");
    serde_json::to_value(&resp).expect("response json")
}

async fn notify(service: &mut LspService<Backend>, req: Request) {
    let _ = service.call(req).await.expect("notify ok");
}

#[tokio::test]
async fn full_lifecycle_initialize_open_completion_shutdown() {
    let (mut service, drainer) = make_service().await;

    // 1. initialize
    let body = call_json(
        &mut service,
        Request::build("initialize")
            .id(1)
            .params(json!({
                "processId": null,
                "rootUri": null,
                "capabilities": {},
            }))
            .finish(),
    )
    .await;
    assert_eq!(body["id"], json!(1));
    assert_eq!(body["jsonrpc"], "2.0");
    let caps = &body["result"]["capabilities"];
    assert!(caps["completionProvider"].is_object());
    assert!(caps["hoverProvider"].is_boolean());

    // 2. initialized (notification)
    notify(
        &mut service,
        Request::build("initialized").params(json!({})).finish(),
    )
    .await;

    // 3. textDocument/didOpen (notification; triggers publishDiagnostics)
    notify(
        &mut service,
        Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": "file:///test.tf",
                    "languageId": "terraform",
                    "version": 1,
                    "text": "variable \"region\" {}\nvariable \"name\" {}\noutput \"x\" { value = var.region }\n"
                }
            }))
            .finish(),
    )
    .await;

    // 4. textDocument/completion at top-level (line 3 column 0)
    let body = call_json(
        &mut service,
        Request::build("textDocument/completion")
            .id(2)
            .params(json!({
                "textDocument": { "uri": "file:///test.tf" },
                "position": { "line": 3, "character": 0 }
            }))
            .finish(),
    )
    .await;
    assert_eq!(body["id"], json!(2));
    let items = body["result"].as_array().expect("items array");
    let labels: Vec<String> = items
        .iter()
        .map(|i| i["label"].as_str().expect("label").to_string())
        .collect();
    assert!(labels.contains(&"resource".to_string()), "got {labels:?}");
    assert!(labels.contains(&"variable".to_string()));

    // 5. shutdown
    let body = call_json(
        &mut service,
        Request::build("shutdown").id(3).finish(),
    )
    .await;
    assert_eq!(body["id"], json!(3));
    assert_eq!(body["result"], json!(null));

    drop(service);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), drainer).await;
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let (mut service, drainer) = make_service().await;

    // Must initialize first; tower-lsp gates most methods behind it.
    let _ = call_json(
        &mut service,
        Request::build("initialize")
            .id(1)
            .params(json!({
                "processId": null,
                "rootUri": null,
                "capabilities": {},
            }))
            .finish(),
    )
    .await;

    let body = call_json(
        &mut service,
        Request::build("terraform/notARealMethod").id(99).finish(),
    )
    .await;
    assert_eq!(body["id"], json!(99));
    // JSON-RPC "Method not found" is -32601.
    assert_eq!(body["error"]["code"], json!(-32601));

    drop(service);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), drainer).await;
}
