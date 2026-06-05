//! Shared in-process LSP test harness — a "mock client" for driving
//! the real `LspService<Backend>` over its loopback socket.
//!
//! Most integration tests build a `Backend` via
//! `LspService::new(Backend::new)` and call handler functions directly,
//! which can't observe server→client traffic (the `publishDiagnostics`
//! notifications a real editor consumes). [`TestClient`] keeps the
//! socket and drains every server-initiated message into a shared
//! buffer, so tests can assert on the exact wire output — including
//! diagnostic lifecycle behaviour that has no return value to check
//! (config-driven republish, file-deletion cleanup, peer refresh).
//!
//! Drive a session with the ergonomic notification/request helpers
//! (`initialize`, `did_open`, `did_change_configuration`, …), let async
//! publishes flush with [`TestClient::settle`], then inspect captured
//! diagnostics with [`TestClient::publishes_for`] /
//! [`TestClient::last_diagnostics`].
//!
//! `#![allow(dead_code)]` because each test binary compiles this module
//! independently and uses only the slice of the API it needs.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tfls_lsp::Backend;
use tokio::sync::Mutex;
use tower::Service;
use tower_lsp::jsonrpc::Request;
use tower_lsp::LspService;

pub type Captured = Arc<Mutex<Vec<Value>>>;

/// An in-process LSP client driving a real `LspService<Backend>` and
/// capturing every server→client message.
pub struct TestClient {
    service: LspService<Backend>,
    captured: Captured,
    drainer: tokio::task::JoinHandle<()>,
    next_id: i64,
}

impl TestClient {
    /// Build a service with a background drainer recording every
    /// server→client message as JSON.
    pub fn new() -> Self {
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
        Self {
            service,
            captured,
            drainer,
            next_id: 1,
        }
    }

    /// Send a request and return the response as JSON.
    pub async fn request(&mut self, method: &'static str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::build(method).id(id).params(params).finish();
        let resp = self
            .service
            .call(req)
            .await
            .expect("service call")
            .expect("response present");
        serde_json::to_value(&resp).expect("response json")
    }

    /// Send a notification (no response expected).
    pub async fn notify(&mut self, method: &'static str, params: Value) {
        let req = Request::build(method).params(params).finish();
        let _ = self.service.call(req).await.expect("notify");
    }

    /// `initialize` + `initialized`. Pass `root_uri` for workspaces that
    /// need a root; `None` for single-file sessions.
    pub async fn initialize(&mut self, root_uri: Option<&str>) -> Value {
        let resp = self
            .request(
                "initialize",
                json!({
                    "processId": null,
                    "rootUri": root_uri,
                    "capabilities": { "textDocument": {}, "workspace": {} }
                }),
            )
            .await;
        self.notify("initialized", json!({})).await;
        resp
    }

    pub async fn did_open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "terraform",
                    "version": 1,
                    "text": text
                }
            }),
        )
        .await;
    }

    /// Full-document replacement (range omitted) — the shape an editor
    /// sends when the whole buffer changes.
    pub async fn did_change_full(&mut self, uri: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        )
        .await;
    }

    pub async fn did_close(&mut self, uri: &str) {
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await;
    }

    /// `workspace/didChangeConfiguration` with the given settings object.
    pub async fn did_change_configuration(&mut self, settings: Value) {
        self.notify(
            "workspace/didChangeConfiguration",
            json!({ "settings": settings }),
        )
        .await;
    }

    /// `workspace/didChangeWatchedFiles`. `changes` is a list of
    /// `(uri, FileChangeType)` where the type is 1=Created, 2=Changed,
    /// 3=Deleted.
    pub async fn did_change_watched_files(&mut self, changes: &[(&str, i64)]) {
        let changes: Vec<Value> = changes
            .iter()
            .map(|(uri, typ)| json!({ "uri": uri, "type": typ }))
            .collect();
        self.notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": changes }),
        )
        .await;
    }

    pub async fn code_action(&mut self, uri: &str, range: Value, diagnostics: Value) -> Value {
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": range,
                "context": { "diagnostics": diagnostics }
            }),
        )
        .await
    }

    /// Let spawned compute/publish work flush before asserting.
    pub async fn settle(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    /// Every `publishDiagnostics` payload for `uri`, oldest first.
    pub async fn publishes_for(&self, uri: &str) -> Vec<Vec<Value>> {
        let lock = self.captured.lock().await;
        lock.iter()
            .filter_map(|msg| {
                if msg.get("method")?.as_str()? != "textDocument/publishDiagnostics" {
                    return None;
                }
                let params = msg.get("params")?;
                if params.get("uri")?.as_str()? != uri {
                    return None;
                }
                params.get("diagnostics")?.as_array().cloned()
            })
            .collect()
    }

    /// The most recent `publishDiagnostics` payload for `uri` (empty if
    /// none captured).
    pub async fn last_diagnostics(&self, uri: &str) -> Vec<Value> {
        self.publishes_for(uri).await.pop().unwrap_or_default()
    }

    /// Count of `publishDiagnostics` notifications captured for `uri`.
    pub async fn publish_count(&self, uri: &str) -> usize {
        self.publishes_for(uri).await.len()
    }

    /// Dispose cleanly — drops the service so the drainer's socket ends.
    pub async fn shutdown(self) {
        drop(self.service);
        let _ = tokio::time::timeout(Duration::from_secs(1), self.drainer).await;
    }
}

/// Whether any diagnostic message contains `substr`.
pub fn any_message_contains(diags: &[Value], substr: &str) -> bool {
    diags.iter().any(|d| {
        d.get("message")
            .and_then(|m| m.as_str())
            .map(|m| m.contains(substr))
            .unwrap_or(false)
    })
}

/// Whether any diagnostic is an "undefined variable `name`".
pub fn contains_undefined_var(diags: &[Value], name: &str) -> bool {
    diags.iter().any(|d| {
        d.get("message")
            .and_then(|m| m.as_str())
            .map(|m| m.contains("undefined variable") && m.contains(name))
            .unwrap_or(false)
    })
}
