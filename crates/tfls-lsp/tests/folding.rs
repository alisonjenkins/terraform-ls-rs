//! Integration tests for foldingRange + selectionRange.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    FoldingRangeParams, PartialResultParams, Position, SelectionRangeParams,
    TextDocumentIdentifier, Url, WorkDoneProgressParams,
};

fn uri() -> Url {
    Url::parse("file:///f.tf").expect("url")
}

fn backend_with(src: &str) -> Backend {
    let (service, _) = LspService::new(Backend::new);
    let inner = service.inner();
    inner
        .state
        .upsert_document(DocumentState::new(uri(), src, 1));
    Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    )
}

#[tokio::test]
async fn folding_emits_ranges_for_multiline_blocks() {
    let backend = backend_with(
        r#"variable "region" {
  type    = string
  default = "us-east-1"
}
resource "aws_instance" "web" {
  ami = "x"
}
"#,
    );

    let folds = tfls_lsp::handlers::folding::folding_range(
        &backend,
        FoldingRangeParams {
            text_document: TextDocumentIdentifier { uri: uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some");

    assert_eq!(folds.len(), 2, "two multi-line blocks");
    assert_eq!(folds[0].start_line, 0);
}

#[tokio::test]
async fn folding_skips_single_line_blocks() {
    let backend = backend_with("variable \"x\" {}\nvariable \"y\" {}\n");
    let folds = tfls_lsp::handlers::folding::folding_range(
        &backend,
        FoldingRangeParams {
            text_document: TextDocumentIdentifier { uri: uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok");
    assert!(folds.is_none());
}

#[tokio::test]
async fn selection_range_walks_from_inner_to_outer() {
    let backend = backend_with(
        r#"resource "aws_instance" "web" {
  ami = "abc"
}
"#,
    );

    // Cursor inside `abc` on line 1.
    let col = "  ami = \"abc\"".find("abc").unwrap() as u32 + 1;
    let ranges = tfls_lsp::handlers::folding::selection_range(
        &backend,
        SelectionRangeParams {
            text_document: TextDocumentIdentifier { uri: uri() },
            positions: vec![Position::new(1, col)],
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some");

    assert_eq!(ranges.len(), 1);
    // Innermost = cursor position (leaf), then attribute range, then block.
    let mut depth = 0;
    let mut cur = Some(&ranges[0]);
    while let Some(r) = cur {
        depth += 1;
        cur = r.parent.as_deref();
    }
    assert!(depth >= 2, "expected at least 2 levels, got {depth}");
}
