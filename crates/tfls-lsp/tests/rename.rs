//! Integration tests for prepareRename / rename.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    PrepareRenameResponse, Position, RenameParams, TextDocumentIdentifier,
    TextDocumentPositionParams, Url, WorkDoneProgressParams,
};

fn uri(s: &str) -> Url {
    Url::parse(s).expect("url")
}

fn backend_with_doc(u: &Url, src: &str) -> Backend {
    let (service, _) = LspService::new(Backend::new);
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

#[tokio::test]
async fn prepare_rename_returns_narrow_range_for_variable_reference() {
    let u = uri("file:///a.tf");
    let src = "variable \"region\" {}\noutput \"x\" { value = var.region }\n";
    let backend = backend_with_doc(&u, src);

    // Cursor on the `region` in `var.region`.
    let line1 = "output \"x\" { value = var.region }";
    let col = line1.find("region").expect("region") as u32;
    let resp = tfls_lsp::handlers::rename::prepare_rename(
        &backend,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: u },
            position: Position::new(1, col + 2),
        },
    )
    .await
    .expect("ok")
    .expect("response");

    match resp {
        PrepareRenameResponse::RangeWithPlaceholder { range, placeholder } => {
            assert_eq!(placeholder, "region");
            assert_eq!(range.start.line, 1);
            assert_eq!(range.end.character - range.start.character, 6);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn rename_variable_updates_definition_and_reference() {
    let u = uri("file:///a.tf");
    let src = "variable \"region\" {}\noutput \"x\" { value = var.region }\n";
    let backend = backend_with_doc(&u, src);

    let col = "output \"x\" { value = var.region }"
        .find("region")
        .expect("region") as u32;
    let edit = tfls_lsp::handlers::rename::rename(
        &backend,
        RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: Position::new(1, col + 2),
            },
            new_name: "where".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("edit");

    let changes = edit.changes.expect("changes");
    let edits = changes.get(&u).expect("edits");
    assert_eq!(edits.len(), 2, "definition + reference");
    for e in edits {
        assert_eq!(e.new_text, "where");
    }
}

#[tokio::test]
async fn rename_returns_none_when_cursor_not_on_symbol() {
    let u = uri("file:///b.tf");
    // Trailing whitespace line gives us a position that can't resolve to
    // any symbol (variable block ends on line 0, cursor is on line 1).
    let backend = backend_with_doc(&u, "variable \"region\" {}\n\n");
    let edit = tfls_lsp::handlers::rename::rename(
        &backend,
        RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u },
                position: Position::new(1, 0),
            },
            new_name: "x".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok");
    assert!(edit.is_none());
}

#[tokio::test]
async fn prepare_rename_works_on_variable_definition_label() {
    let u = uri("file:///def.tf");
    let src = "variable \"region\" {}\noutput \"x\" { value = var.region }\n";
    let backend = backend_with_doc(&u, src);

    // Cursor on the `region` in the definition label `variable "region"`.
    // Column 10 = start of `region` (inside quotes: `variable "|region"`).
    let resp = tfls_lsp::handlers::rename::prepare_rename(
        &backend,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: u },
            position: Position::new(0, 12),
        },
    )
    .await
    .expect("ok")
    .expect("response");

    match resp {
        PrepareRenameResponse::RangeWithPlaceholder { range, placeholder } => {
            assert_eq!(placeholder, "region");
            // Range should cover just the label text, not the surrounding quotes.
            assert_eq!(range.start, Position::new(0, 10));
            assert_eq!(range.end, Position::new(0, 16));
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[tokio::test]
async fn rename_from_variable_definition_label_updates_both() {
    let u = uri("file:///def-rename.tf");
    let src = "variable \"region\" {}\noutput \"x\" { value = var.region }\n";
    let backend = backend_with_doc(&u, src);

    // Cursor on the label in `variable "region"`.
    let edit = tfls_lsp::handlers::rename::rename(
        &backend,
        RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: Position::new(0, 12),
            },
            new_name: "where".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("edit");

    let changes = edit.changes.expect("changes");
    let edits = changes.get(&u).expect("edits");
    assert_eq!(edits.len(), 2, "definition + reference both get renamed");
    for e in edits {
        assert_eq!(e.new_text, "where");
    }
}

#[tokio::test]
async fn rename_affects_all_references() {
    let u = uri("file:///c.tf");
    let src = r#"variable "x" {}
output "a" { value = var.x }
output "b" { value = var.x }
output "c" { value = var.x }
"#;
    let backend = backend_with_doc(&u, src);
    let col = "output \"a\" { value = var.x }".find("var.x").unwrap() as u32 + 4;
    let edit = tfls_lsp::handlers::rename::rename(
        &backend,
        RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: Position::new(1, col),
            },
            new_name: "y".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("edit");

    let edits = edit.changes.unwrap().remove(&u).unwrap();
    // 1 definition + 3 references = 4.
    assert_eq!(edits.len(), 4);
}
