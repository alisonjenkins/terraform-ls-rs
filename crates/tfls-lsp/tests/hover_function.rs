//! Tests for hover on interpolation function calls.
//!
//! Function signatures are loaded on startup via `install_functions` from
//! either the provider CLI or the bundled snapshot. The hover handler
//! must locate the function name at the cursor (or the enclosing call when
//! the cursor is between the parentheses) and render the signature.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_schema::bundled_functions;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    HoverParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, Url,
    WorkDoneProgressParams,
};

fn uri(path: &str) -> Url {
    Url::parse(path).expect("valid url")
}

fn backend_with(src: &str, u: &Url) -> Backend {
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    inner
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));
    // Seed bundled functions so lookups can succeed.
    if let Ok(schema) = bundled_functions() {
        inner.state.install_functions(schema);
    }
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
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")?;
    match hover.contents {
        tower_lsp::lsp_types::HoverContents::Markup(m) => Some(m.value),
        other => panic!("expected markup, got {other:?}"),
    }
}

#[tokio::test]
async fn hover_on_function_name_shows_signature_and_description() {
    let u = uri("file:///a.tf");
    let src = "output \"x\" { value = tostring(var.y) }\n";
    let b = backend_with(src, &u);

    // Cursor on `tostring` (middle of the name).
    let col = src.find("tostring").unwrap() as u32 + 3;
    let md = hover_markdown(&b, &u, Position::new(0, col))
        .await
        .expect("some hover");

    assert!(md.contains("tostring"), "missing function name: {md}");
    assert!(
        md.to_lowercase().contains("converts") || md.to_lowercase().contains("string"),
        "missing description from bundled schema: {md}"
    );
}

#[tokio::test]
async fn hover_inside_call_parens_finds_function() {
    let u = uri("file:///b.tf");
    let src = "output \"x\" { value = length(var.names) }\n";
    let b = backend_with(src, &u);

    // Cursor inside the parens — between the `(` and the `)`.
    let col = src.find("var.names").unwrap() as u32;
    let md = hover_markdown(&b, &u, Position::new(0, col))
        .await
        .expect("some hover");

    assert!(md.contains("length"), "expected length signature: {md}");
}

#[tokio::test]
async fn hover_on_nested_call_prefers_innermost() {
    let u = uri("file:///c.tf");
    let src = "output \"x\" { value = format(\"%d\", length(var.names)) }\n";
    let b = backend_with(src, &u);

    // Cursor on `length`, which is nested inside format(...).
    let col = src.find("length").unwrap() as u32 + 2;
    let md = hover_markdown(&b, &u, Position::new(0, col))
        .await
        .expect("some hover");

    assert!(md.contains("length"), "expected length hover: {md}");
    assert!(
        !md.starts_with("**function** `format`"),
        "inner call should win over outer: {md}"
    );
}

#[tokio::test]
async fn hover_on_plain_identifier_is_not_a_function_hover() {
    let u = uri("file:///d.tf");
    let src = "variable \"region\" {}\n";
    let b = backend_with(src, &u);

    // Cursor on `region` inside the label — this is a variable definition,
    // not a function call. Hover should still return a non-function result
    // (the existing symbol-level fallback). The function branch must NOT
    // return something here.
    let md = hover_markdown(&b, &u, Position::new(0, 12))
        .await
        .expect("some hover");

    assert!(
        !md.contains("**function**"),
        "variable label should not produce a function hover: {md}"
    );
    assert!(md.contains("variable"), "expected symbol hover: {md}");
}

#[tokio::test]
async fn function_hover_on_unknown_function_returns_none() {
    // Direct probe of `function_hover` (bypasses the fallback chain in
    // `navigation::hover`). The function-level handler must not invent a
    // hover for a name that isn't in `state.functions`.
    let u = uri("file:///e.tf");
    let src = "output \"x\" { value = totally_made_up_fn(var.y) }\n";
    let b = backend_with(src, &u);

    let col = src.find("totally_made_up_fn").unwrap() as u32 + 2;
    let doc = b.state.documents.get(&u).expect("doc");
    let hover = tfls_lsp::handlers::hover_function::function_hover(
        &b.state,
        &doc,
        Position::new(0, col),
    );
    assert!(
        hover.is_none(),
        "unknown fn should not produce a function hover"
    );
}
