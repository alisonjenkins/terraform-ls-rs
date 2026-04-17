//! Integration tests for the completion handler.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_schema::ProviderSchemas;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    CompletionContext, CompletionParams, CompletionResponse, CompletionTriggerKind,
    PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams, Url,
    WorkDoneProgressParams,
};

fn uri(path: &str) -> Url {
    Url::parse(path).expect("valid url")
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
    let schema: ProviderSchemas = sonic_rs::from_str(
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
                                "ami":           { "type": "string", "required": true,  "description": "AMI ID" },
                                "instance_type": { "type": "string", "optional": true },
                                "tags":          { "type": ["map", "string"], "optional": true }
                            }
                        }
                    }
                },
                "data_source_schemas": {
                    "aws_ami": { "version": 0, "block": {} }
                }
            }
        }
    }"#,
    )
    .expect("parse schema");
    backend.state.install_schemas(schema);
}

fn make_params(u: &Url, pos: Position) -> CompletionParams {
    CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            position: pos,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: Some(CompletionContext {
            trigger_kind: CompletionTriggerKind::INVOKED,
            trigger_character: None,
        }),
    }
}

fn labels(resp: CompletionResponse) -> Vec<String> {
    match resp {
        CompletionResponse::Array(items) => items.into_iter().map(|i| i.label).collect(),
        CompletionResponse::List(list) => list.items.into_iter().map(|i| i.label).collect(),
    }
}

#[tokio::test]
async fn top_level_suggests_block_keywords() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("", &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"resource".to_string()));
    assert!(ls.contains(&"variable".to_string()));
    assert!(ls.contains(&"module".to_string()));
}

#[tokio::test]
async fn resource_type_position_returns_schema_types() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("resource \"", &u);
    install_aws_schema(&backend);

    // Cursor immediately after the opening quote.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 10)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["aws_instance".to_string()]);
}

#[tokio::test]
async fn resource_body_suggests_attributes_from_schema() {
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"web\" {\n\n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    // Cursor on the empty line inside the block.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"ami".to_string()));
    assert!(ls.contains(&"instance_type".to_string()));
    assert!(ls.contains(&"tags".to_string()));
}

#[tokio::test]
async fn variable_ref_suggests_defined_variables() {
    // Simulate realistic flow: start with a valid file whose symbols
    // are indexed, then type `var.` to trigger a momentary parse
    // failure. The last-good symbol table keeps completion working.
    let u = uri("file:///a.tf");
    let valid_src = "variable \"region\" {}\nvariable \"name\" {}\noutput \"x\" { value = var.region }\n";
    let backend = fresh_backend(valid_src, &u);

    // Overwrite rope to the broken state and reparse — mimicking a
    // didChange event that leaves the doc momentarily un-parseable.
    {
        if let Some(mut doc) = backend.state.documents.get_mut(&u) {
            doc.rope = ropey::Rope::from_str(
                "variable \"region\" {}\nvariable \"name\" {}\noutput \"x\" { value = var.",
            );
        }
        backend.state.reparse_document(&u);
    }

    let last_line = "output \"x\" { value = var.";
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, last_line.len() as u32)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    // Sorted ascii: name, region.
    assert_eq!(ls, vec!["name".to_string(), "region".to_string()]);
}

#[tokio::test]
async fn completion_without_schema_returns_none_for_resource_type() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("resource \"", &u);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 10)),
    )
    .await
    .expect("ok");
    assert!(resp.is_none(), "no schemas installed -> no suggestions");
}
