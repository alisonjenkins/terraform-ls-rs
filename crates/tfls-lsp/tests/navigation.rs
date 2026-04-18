//! Integration test exercising the navigation handlers end-to-end
//! through the [`Backend`] — no LSP wire protocol, just the handler
//! calls with fabricated params.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, HoverParams, PartialResultParams, Position,
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentPositionParams, Url,
    WorkDoneProgressParams,
};

fn uri(path: &str) -> Url {
    Url::parse(path).expect("valid url")
}

fn backend_with(src: &str, u: &Url) -> Backend {
    let (service, _socket) = LspService::new(Backend::new);
    let backend = service.inner();
    // Directly populate state; the handlers operate on StateStore, not raw RPC.
    backend
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));
    // LspService doesn't let us extract owned Backend, so clone Arc state into
    // a fresh Backend struct for the test. (In production, the service owns it.)
    Backend::with_shared_state(
        backend.client.clone(),
        backend.state.clone(),
        backend.jobs.clone(),
    )
}

#[tokio::test]
async fn goto_definition_finds_variable() {
    let u = uri("file:///test.tf");
    let src = "variable \"region\" { default = \"us-east-1\" }\noutput \"x\" { value = var.region }\n";
    let backend = backend_with(src, &u);

    // Cursor on "region" inside var.region (line 1, after `var.`).
    let pos = Position::new(1, 25);
    let result = tfls_lsp::handlers::navigation::goto_definition(
        &backend,
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok");

    let locations = match result {
        Some(GotoDefinitionResponse::Array(v)) => v,
        other => panic!("expected Array response, got {other:?}"),
    };
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, u);
    // Variable is on line 0.
    assert_eq!(locations[0].range.start.line, 0);
}

#[tokio::test]
async fn references_includes_declaration_when_requested() {
    let u = uri("file:///refs.tf");
    let src = r#"variable "region" {}
output "a" { value = var.region }
output "b" { value = var.region }
"#;
    let backend = backend_with(src, &u);

    let pos = Position::new(1, 25); // cursor on first var.region
    let result = tfls_lsp::handlers::navigation::references(
        &backend,
        ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        },
    )
    .await
    .expect("ok");

    let locations = result.expect("locations present");
    // 1 declaration + 2 references.
    assert_eq!(locations.len(), 3);
}

#[tokio::test]
async fn hover_returns_kind_and_name() {
    let u = uri("file:///h.tf");
    let src = r#"variable "region" {}
output "x" { value = var.region }
"#;
    let backend = backend_with(src, &u);

    let pos = Position::new(1, 25);
    let hover = tfls_lsp::handlers::navigation::hover(
        &backend,
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok");

    let hover = hover.expect("some hover");
    let markdown = match hover.contents {
        tower_lsp::lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup, got {other:?}"),
    };
    assert!(markdown.contains("variable"), "got: {markdown}");
    assert!(markdown.contains("region"), "got: {markdown}");
}

#[tokio::test]
async fn hover_works_on_definition_label() {
    // Regression test: prior to the key_at_cursor refactor, hover would return
    // None when the cursor was on a block label. Now it should behave the same
    // as when the cursor is on a reference.
    let u = uri("file:///def.tf");
    let src = r#"variable "region" {}
"#;
    let backend = backend_with(src, &u);

    // Cursor on `region` inside `variable "region"` — column 12 puts us
    // inside the quoted label.
    let hover = tfls_lsp::handlers::navigation::hover(
        &backend,
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: Position::new(0, 12),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some hover");

    let markdown = match hover.contents {
        tower_lsp::lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup, got {other:?}"),
    };
    assert!(markdown.contains("variable"), "got: {markdown}");
    assert!(markdown.contains("region"), "got: {markdown}");
}

#[tokio::test]
async fn goto_definition_on_nothing_returns_none() {
    let u = uri("file:///empty.tf");
    let backend = backend_with("", &u);

    let result = tfls_lsp::handlers::navigation::goto_definition(
        &backend,
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u },
                position: Position::new(0, 0),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok");

    assert!(result.is_none());
}
