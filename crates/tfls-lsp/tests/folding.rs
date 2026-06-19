//! Integration tests for foldingRange + selectionRange.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lsp_types::{
    FoldingRangeParams, PartialResultParams, Position, SelectionRangeParams,
    TextDocumentIdentifier, WorkDoneProgressParams,
};
use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp_server::LspService;
use url::Url;

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
            text_document: TextDocumentIdentifier {
                uri: tfls_core::uri::url_to_uri(&uri()),
            },
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

async fn folds_for(src: &str) -> Vec<lsp_types::FoldingRange> {
    let backend = backend_with(src);
    tfls_lsp::handlers::folding::folding_range(
        &backend,
        FoldingRangeParams {
            text_document: TextDocumentIdentifier {
                uri: tfls_core::uri::url_to_uri(&uri()),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .unwrap_or_default()
}

fn has_fold_starting_at(folds: &[lsp_types::FoldingRange], line: u32) -> bool {
    folds
        .iter()
        .any(|f| f.start_line == line && f.end_line > f.start_line)
}

#[tokio::test]
async fn folding_folds_object_in_locals() {
    // locals block (line 0), object value starts line 1.
    let folds = folds_for("locals {\n  tags = {\n    a = 1\n    b = 2\n  }\n}\n").await;
    assert!(has_fold_starting_at(&folds, 0), "locals block fold missing");
    assert!(
        has_fold_starting_at(&folds, 1),
        "object value fold missing: {folds:?}"
    );
}

#[tokio::test]
async fn folding_folds_list_in_locals() {
    let folds = folds_for("locals {\n  names = [\n    \"a\",\n    \"b\",\n  ]\n}\n").await;
    assert!(
        has_fold_starting_at(&folds, 1),
        "list value fold missing: {folds:?}"
    );
}

#[tokio::test]
async fn folding_folds_nested_object_in_list() {
    // list starts line 1, inner object starts line 2.
    let src = "locals {\n  items = [\n    {\n      k = 1\n    },\n  ]\n}\n";
    let folds = folds_for(src).await;
    assert!(
        has_fold_starting_at(&folds, 1),
        "outer list fold missing: {folds:?}"
    );
    assert!(
        has_fold_starting_at(&folds, 2),
        "inner object fold missing: {folds:?}"
    );
}

#[tokio::test]
async fn folding_folds_heredoc() {
    let src = "resource \"x\" \"y\" {\n  user_data = <<-EOT\n    line1\n    line2\n  EOT\n}\n";
    let folds = folds_for(src).await;
    assert!(
        has_fold_starting_at(&folds, 1),
        "heredoc fold missing: {folds:?}"
    );
}

#[tokio::test]
async fn folding_folds_funccall_args() {
    let src = "locals {\n  merged = merge(\n    var.a,\n    var.b,\n  )\n}\n";
    let folds = folds_for(src).await;
    assert!(
        has_fold_starting_at(&folds, 1),
        "func-call args fold missing: {folds:?}"
    );
}

#[tokio::test]
async fn folding_does_not_swallow_blank_line_between_blocks() {
    // Two blocks separated by ONE blank line (line 3). The first fold must
    // end on the closing-brace line (2), never extend into the blank
    // separator — otherwise folded blocks render flush with no gap.
    let src = "variable \"a\" {\n  type = string\n}\n\nvariable \"b\" {\n  type = string\n}\n";
    let folds = folds_for(src).await;
    let first = folds
        .iter()
        .find(|f| f.start_line == 0)
        .expect("first block fold");
    assert_eq!(
        first.end_line, 2,
        "fold swallowed the blank line: {folds:?}"
    );
}

#[tokio::test]
async fn folding_does_not_emit_duplicate_overlapping_folds() {
    // `type = object({...})` parses as a FuncCall wrapping an Object; both
    // span the same lines. Emitting two identical folds confuses clients'
    // nested-fold engines — collapse them to one per line range.
    let src = "variable \"ad\" {\n  type = object({\n    u = string\n    p = string\n  })\n}\n";
    let folds = folds_for(src).await;
    let mut seen = std::collections::HashSet::new();
    for f in &folds {
        assert!(
            seen.insert((f.start_line, f.end_line)),
            "duplicate fold for lines {}..{}: {folds:?}",
            f.start_line,
            f.end_line
        );
    }
}

#[tokio::test]
async fn folding_skips_single_line_object() {
    // Object value entirely on one line — no expression fold; only the
    // multi-line locals block itself folds.
    // Object value entirely on one line — no expression fold; only the
    // multi-line locals block itself folds.
    let folds = folds_for("locals {\n  tags = { a = 1, b = 2 }\n}\n").await;
    assert!(has_fold_starting_at(&folds, 0), "locals block fold missing");
    assert!(
        !has_fold_starting_at(&folds, 1),
        "single-line object should not fold: {folds:?}"
    );
}

#[tokio::test]
async fn folding_skips_single_line_blocks() {
    let backend = backend_with("variable \"x\" {}\nvariable \"y\" {}\n");
    let folds = tfls_lsp::handlers::folding::folding_range(
        &backend,
        FoldingRangeParams {
            text_document: TextDocumentIdentifier {
                uri: tfls_core::uri::url_to_uri(&uri()),
            },
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
            text_document: TextDocumentIdentifier {
                uri: tfls_core::uri::url_to_uri(&uri()),
            },
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
