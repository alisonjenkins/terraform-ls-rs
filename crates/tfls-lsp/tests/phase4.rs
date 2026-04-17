//! Phase-4 integration tests: formatting, document links, code actions,
//! undefined-reference + schema diagnostics.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_lsp::handlers::document::compute_diagnostics;
use tfls_schema::ProviderSchemas;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    CodeActionContext, CodeActionOrCommand, CodeActionParams, DiagnosticSeverity,
    DocumentFormattingParams, DocumentLinkParams, FormattingOptions, PartialResultParams,
    Position, Range, TextDocumentIdentifier, Url, WorkDoneProgressParams,
};

fn uri(s: &str) -> Url {
    Url::parse(s).expect("valid url")
}

fn fresh_backend(src: &str, u: &Url) -> Backend {
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

fn install_aws_schema(backend: &Backend) {
    let schemas: ProviderSchemas = sonic_rs::from_str(
        r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/hashicorp/aws": {
                "provider": { "version": 0, "block": {} },
                "resource_schemas": {
                    "aws_instance": {
                        "version": 1,
                        "block": {
                            "attributes": {
                                "ami":           { "type": "string", "required": true },
                                "instance_type": { "type": "string", "optional": true },
                                "legacy_flag":   { "type": "bool", "optional": true, "deprecated": true }
                            }
                        }
                    }
                },
                "data_source_schemas": {}
            }
        }
    }"#,
    )
    .expect("schemas parse");
    backend.state.install_schemas(schemas);
}

#[tokio::test]
async fn formatting_fixes_trailing_whitespace_and_blank_lines() {
    let u = uri("file:///a.tf");
    let src = "variable \"x\" {}   \n\n\n\nvariable \"y\" {}\n";
    let backend = fresh_backend(src, &u);

    let edits = tfls_lsp::handlers::formatting::formatting(
        &backend,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            options: FormattingOptions::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("edits");

    assert_eq!(edits.len(), 1);
    assert!(edits[0].new_text.contains("variable \"x\" {}"));
    assert!(!edits[0].new_text.contains("  \n"));
}

#[tokio::test]
async fn formatting_returns_empty_for_already_formatted_source() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("variable \"x\" {}\n", &u);
    let edits = tfls_lsp::handlers::formatting::formatting(
        &backend,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri: u },
            options: FormattingOptions::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some edits vec");
    assert!(edits.is_empty(), "no changes needed");
}

#[tokio::test]
async fn document_links_point_to_registry_docs() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend(
        r#"resource "aws_instance" "web" { ami = "x" }"#,
        &u,
    );
    install_aws_schema(&backend);

    let links = tfls_lsp::handlers::document_link::document_link(
        &backend,
        DocumentLinkParams {
            text_document: TextDocumentIdentifier { uri: u },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some links");
    assert_eq!(links.len(), 1);
    let target = links[0].target.as_ref().expect("target");
    assert!(
        target
            .as_str()
            .ends_with("/providers/hashicorp/aws/latest/docs/resources/aws_instance")
    );
}

#[tokio::test]
async fn diagnostics_include_undefined_and_schema_checks() {
    let u = uri("file:///a.tf");
    let src = r#"resource "aws_instance" "web" {
  instance_type = "t3.micro"
  not_there = 1
  legacy_flag = true
}
output "x" { value = var.missing }
"#;
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    let diags = compute_diagnostics(&backend.state, &u);

    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("undefined variable") && d.message.contains("missing")),
        "undefined var: {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("missing required attribute") && d.message.contains("ami")),
        "missing required: {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("unknown attribute") && d.message.contains("not_there")),
        "unknown attr: {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.message.contains("deprecated")
            && d.severity == Some(DiagnosticSeverity::WARNING)),
        "deprecation: {diags:?}"
    );
}

#[tokio::test]
async fn code_action_inserts_missing_required_attribute() {
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  instance_type = \"t3.micro\"\n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    let diags = compute_diagnostics(&backend.state, &u);
    let missing_req = diags
        .iter()
        .find(|d| d.message.contains("missing required attribute") && d.message.contains("ami"))
        .cloned()
        .expect("should have missing-required diag");

    let actions = tfls_lsp::handlers::code_action::code_action(
        &backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 10),
            },
            context: CodeActionContext {
                diagnostics: vec![missing_req],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("actions");

    assert_eq!(actions.len(), 1);
    let action = match &actions[0] {
        CodeActionOrCommand::CodeAction(a) => a,
        other => panic!("expected CodeAction, got {other:?}"),
    };
    let edit = action.edit.as_ref().expect("edit");
    let changes = edit.changes.as_ref().expect("changes");
    let edits = changes.get(&u).expect("edits for this uri");
    assert_eq!(edits.len(), 1);
    assert!(
        edits[0].new_text.contains("ami = \"\""),
        "new text: {:?}",
        edits[0].new_text
    );
}
