//! Tests for hover on type-constraint keywords inside `type = …`.
//!
//! `optional`, `object`, `list`, the primitives, etc. aren't functions or
//! symbols, so they get a dedicated hover path. The handler must only fire
//! inside a type expression — an identifier merely sharing the name must not
//! pick up a type-constraint card.

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

/// Line/character position of `needle`'s first occurrence in `src`, offset
/// into the needle by `delta` characters. Keeps the tests written against
/// realistic multi-line HCL (the type-expression detector is line-aware:
/// block openers are expected one-per-line, as real Terraform writes them).
fn pos_of(src: &str, needle: &str, delta: u32) -> Position {
    let byte = src.find(needle).expect("needle present") + delta as usize;
    let before = &src[..byte];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let character = before[line_start..].chars().count() as u32;
    Position::new(line, character)
}

#[tokio::test]
async fn hover_on_optional_explains_null_when_omitted() {
    let u = uri("file:///opt.tf");
    let src = "variable \"x\" {\n  type = object({\n    p = optional(string)\n  })\n}\n";
    let b = backend_with(src, &u);

    let md = hover_markdown(&b, &u, pos_of(src, "optional", 2))
        .await
        .expect("some hover");

    assert!(md.contains("optional"), "missing keyword: {md}");
    assert!(
        md.contains("null"),
        "must explain omitted-value behaviour: {md}"
    );
}

#[tokio::test]
async fn hover_on_object_constructor() {
    let u = uri("file:///obj.tf");
    let src = "variable \"x\" {\n  type = object({ name = string })\n}\n";
    let b = backend_with(src, &u);

    let md = hover_markdown(&b, &u, pos_of(src, "object", 2))
        .await
        .expect("some hover");
    assert!(md.contains("object"), "missing object description: {md}");
}

#[tokio::test]
async fn hover_on_primitive_string() {
    let u = uri("file:///str.tf");
    let src = "variable \"x\" {\n  type = string\n}\n";
    let b = backend_with(src, &u);

    let md = hover_markdown(&b, &u, pos_of(src, "= string", 3))
        .await
        .expect("some hover");
    assert!(
        md.to_lowercase().contains("string"),
        "missing string description: {md}"
    );
    assert!(md.contains("type constraint"), "missing header: {md}");
}

#[tokio::test]
async fn hover_on_nested_optional_list_default() {
    let u = uri("file:///nest.tf");
    let src =
        "variable \"x\" {\n  type = object({\n    tags = optional(list(string), [])\n  })\n}\n";
    let b = backend_with(src, &u);

    let md = hover_markdown(&b, &u, pos_of(src, "optional", 1))
        .await
        .expect("some hover");
    assert!(md.contains("optional"), "missing keyword: {md}");
    // The empty-list misconception must be addressed in the card.
    assert!(md.contains("[]"), "should mention explicit default: {md}");
}

#[tokio::test]
async fn keyword_outside_type_expr_is_not_a_type_constraint_hover() {
    let u = uri("file:///guard.tf");
    // `string` here is the variable's name, not a type keyword.
    let src = "variable \"string\" {\n  default = 1\n}\n";
    let b = backend_with(src, &u);

    let md = hover_markdown(&b, &u, pos_of(src, "\"string\"", 2)).await;
    // It may fall through to the symbol hover, but it must NOT be the
    // type-constraint card.
    if let Some(md) = md {
        assert!(
            !md.contains("type constraint"),
            "variable label should not produce a type-constraint hover: {md}"
        );
    }
}
