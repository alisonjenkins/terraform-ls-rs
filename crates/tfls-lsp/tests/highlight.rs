//! Integration test for documentHighlight.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    DocumentHighlightKind, DocumentHighlightParams, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, Url, WorkDoneProgressParams,
};

#[tokio::test]
async fn highlights_definition_as_write_and_refs_as_read() {
    let u = Url::parse("file:///a.tf").expect("url");
    let src = r#"variable "region" {}
output "a" { value = var.region }
output "b" { value = var.region }
"#;
    let (service, _) = LspService::new(Backend::new);
    let inner = service.inner();
    inner
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));
    let backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );

    // Cursor inside `var.region` on line 1.
    let col = "output \"a\" { value = var.region }"
        .find("region")
        .unwrap() as u32
        + 2;
    let highlights = tfls_lsp::handlers::highlight::document_highlight(
        &backend,
        DocumentHighlightParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: Position::new(1, col),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some");

    // 1 Write (the variable block) + 2 Read (references) = 3.
    assert_eq!(highlights.len(), 3);
    let writes = highlights
        .iter()
        .filter(|h| h.kind == Some(DocumentHighlightKind::WRITE))
        .count();
    let reads = highlights
        .iter()
        .filter(|h| h.kind == Some(DocumentHighlightKind::READ))
        .count();
    assert_eq!(writes, 1);
    assert_eq!(reads, 2);
}
