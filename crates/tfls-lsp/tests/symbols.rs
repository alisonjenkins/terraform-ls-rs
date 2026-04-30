//! Integration tests for documentSymbol + workspace/symbol.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    DocumentSymbolParams, DocumentSymbolResponse, PartialResultParams, TextDocumentIdentifier,
    Url, WorkDoneProgressParams, WorkspaceSymbolParams,
};

fn uri(s: &str) -> Url {
    Url::parse(s).expect("valid url")
}

fn backend_with_doc(u: &Url, src: &str) -> Backend {
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

#[tokio::test]
async fn document_symbol_returns_nested_response() {
    let u = uri("file:///a.tf");
    let backend = backend_with_doc(
        &u,
        r#"variable "region" {}
local "env" { value = "prod" }
output "id" { value = 1 }
resource "aws_instance" "web" { ami = "x" }
data "aws_ami" "u" { owners = ["x"] }
module "net" { source = "./n" }
"#,
    );

    let resp = tfls_lsp::handlers::symbols::document_symbol(
        &backend,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some");

    let symbols = match resp {
        DocumentSymbolResponse::Nested(v) => v,
        other => panic!("expected nested, got {other:?}"),
    };
    // variable, output, resource, data, module = 5 (locals block has no attributes — `local` as a block name is not standard, so it won't parse into locals map)
    assert!(symbols.len() >= 4, "got {}: {:?}", symbols.len(), symbols);
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"region"));
    assert!(names.contains(&"id"));
    assert!(names.iter().any(|n| n == &"web" || n == &"aws_instance"));
}

#[tokio::test]
async fn document_symbol_sorts_by_position() {
    let u = uri("file:///b.tf");
    let backend = backend_with_doc(
        &u,
        r#"output "z" { value = 1 }
variable "a" {}
"#,
    );
    let resp = tfls_lsp::handlers::symbols::document_symbol(
        &backend,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: u },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some");

    let symbols = match resp {
        DocumentSymbolResponse::Nested(v) => v,
        _ => panic!("nested"),
    };
    // Output "z" comes before variable "a" in source; ordering must follow source.
    assert_eq!(symbols[0].name, "z");
    assert_eq!(symbols[1].name, "a");
}

#[tokio::test]
async fn document_symbol_none_for_empty_document() {
    let u = uri("file:///e.tf");
    let backend = backend_with_doc(&u, "");
    let resp = tfls_lsp::handlers::symbols::document_symbol(
        &backend,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: u },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok");
    assert!(resp.is_none());
}

#[tokio::test]
async fn workspace_symbol_fuzzy_matches_across_documents() {
    let u1 = uri("file:///a.tf");
    let u2 = uri("file:///b.tf");
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    inner.state.upsert_document(DocumentState::new(
        u1.clone(),
        r#"variable "region" {}"#,
        1,
    ));
    inner.state.upsert_document(DocumentState::new(
        u2.clone(),
        r#"resource "aws_instance" "web" { ami = "x" }"#,
        1,
    ));
    let backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );

    let resp = tfls_lsp::handlers::symbols::workspace_symbol(
        &backend,
        WorkspaceSymbolParams {
            query: "reg".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some");

    assert_eq!(resp.len(), 1);
    assert_eq!(resp[0].name, "region");
}

#[tokio::test]
async fn workspace_symbol_empty_query_returns_everything() {
    let u = uri("file:///x.tf");
    let backend = backend_with_doc(
        &u,
        r#"variable "a" {}
variable "b" {}
variable "c" {}
"#,
    );

    let resp = tfls_lsp::handlers::symbols::workspace_symbol(
        &backend,
        WorkspaceSymbolParams {
            query: String::new(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some");
    assert_eq!(resp.len(), 3);
}

#[tokio::test]
async fn workspace_symbol_returns_none_when_nothing_matches() {
    let u = uri("file:///z.tf");
    let backend = backend_with_doc(&u, r#"variable "region" {}"#);

    let resp = tfls_lsp::handlers::symbols::workspace_symbol(
        &backend,
        WorkspaceSymbolParams {
            query: "notthere".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok");
    assert!(resp.is_none());
}

#[tokio::test]
async fn workspace_symbol_finds_provider_function_calls() {
    let u = uri("file:///proj/main.tf");
    let backend = backend_with_doc(
        &u,
        "output \"x\" {\n  value = provider::aws::trim_prefix(\"foo\")\n}\n\
         output \"y\" {\n  value = provider::aws::arn_parse(\"bar\")\n}\n",
    );
    let resp = tfls_lsp::handlers::symbols::workspace_symbol(
        &backend,
        WorkspaceSymbolParams {
            query: "trim".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some");

    let names: Vec<String> = resp.iter().map(|s| s.name.clone()).collect();
    assert!(
        names
            .iter()
            .any(|n| n == "provider::aws::trim_prefix"),
        "got: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|n| n == "provider::aws::arn_parse"),
        "non-matching call leaked: {names:?}"
    );
}
