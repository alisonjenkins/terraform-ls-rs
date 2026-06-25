//! Tests for hover on Terraform built-in named values.
//!
//! `path.*`, `terraform.workspace`, `count.index`, `each.*`, and `self`
//! aren't declared anywhere, so they get a dedicated hover path rather than
//! the symbol-table fallback.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lsp_types::{
    HoverParams, Position, TextDocumentIdentifier, TextDocumentPositionParams,
    WorkDoneProgressParams,
};
use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp_server::LspService;
use url::Url;

fn uri(path: &str) -> Url {
    Url::parse(path).expect("valid url")
}

fn backend_with(src: &str, u: &Url) -> Backend {
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    inner
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));
    Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    )
}

async fn hover_markdown(backend: &Backend, u: &Url, pos: Position) -> Option<String> {
    let hover = tfls_lsp::handlers::navigation::hover(
        backend,
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: tfls_core::uri::url_to_uri(u),
                },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")?;
    match hover.contents {
        lsp_types::HoverContents::Markup(m) => Some(m.value),
        other => panic!("expected markup, got {other:?}"),
    }
}

fn col_of(src: &str, needle: &str, delta: u32) -> u32 {
    src.find(needle).expect("needle present") as u32 + delta
}

#[tokio::test]
async fn hover_on_path_module_member() {
    let u = uri("file:///pm.tf");
    let src = "output \"x\" { value = path.module }\n";
    let b = backend_with(src, &u);

    // Cursor on the `module` segment.
    let col = col_of(src, "path.module", 7);
    let md = hover_markdown(&b, &u, Position::new(0, col))
        .await
        .expect("some hover");
    assert!(md.contains("path.module"), "missing member: {md}");
    assert!(
        md.to_lowercase().contains("path"),
        "missing description: {md}"
    );
}

#[tokio::test]
async fn hover_on_path_head_resolves_member() {
    let u = uri("file:///ph.tf");
    let src = "output \"x\" { value = path.root }\n";
    let b = backend_with(src, &u);

    // Cursor on the `path` head — handler reads the following `.root`.
    let col = col_of(src, "path.root", 1);
    let md = hover_markdown(&b, &u, Position::new(0, col))
        .await
        .expect("some hover");
    assert!(md.contains("path.root"), "head should resolve member: {md}");
}

#[tokio::test]
async fn hover_on_terraform_workspace() {
    let u = uri("file:///tw.tf");
    let src = "output \"x\" { value = terraform.workspace }\n";
    let b = backend_with(src, &u);

    let col = col_of(src, "workspace", 2);
    let md = hover_markdown(&b, &u, Position::new(0, col))
        .await
        .expect("some hover");
    assert!(md.contains("terraform.workspace"), "missing member: {md}");
}

#[tokio::test]
async fn hover_on_count_index() {
    let u = uri("file:///ci.tf");
    let src = "output \"x\" { value = count.index }\n";
    let b = backend_with(src, &u);

    let col = col_of(src, "count.index", 7);
    let md = hover_markdown(&b, &u, Position::new(0, col))
        .await
        .expect("some hover");
    assert!(md.contains("count.index"), "missing member: {md}");
}

#[tokio::test]
async fn hover_on_each_value() {
    let u = uri("file:///ev.tf");
    let src = "output \"x\" { value = each.value }\n";
    let b = backend_with(src, &u);

    let col = col_of(src, "each.value", 6);
    let md = hover_markdown(&b, &u, Position::new(0, col))
        .await
        .expect("some hover");
    assert!(md.contains("each.value"), "missing member: {md}");
}

#[tokio::test]
async fn hover_on_self() {
    let u = uri("file:///self.tf");
    let src = "output \"x\" { value = self.private_ip }\n";
    let b = backend_with(src, &u);

    let col = col_of(src, "self", 1);
    let md = hover_markdown(&b, &u, Position::new(0, col))
        .await
        .expect("some hover");
    assert!(
        md.to_lowercase().contains("self"),
        "missing self description: {md}"
    );
}
