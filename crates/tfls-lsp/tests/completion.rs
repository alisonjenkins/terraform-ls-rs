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

// Regression: when the cursor is inside an already-closed resource
// type label (e.g. while editing the `${1:type}` placeholder of the
// top-level `resource` snippet), emit the type name as plain text
// only. Emitting the full scaffold duplicates the outer snippet's
// closing quote + name label + body and produces malformed code.
#[tokio::test]
async fn resource_type_completion_in_closed_label_inserts_bare_name() {
    let u = uri("file:///a.tf");
    // Mirrors post-snippet state: outer resource scaffold already
    // placed `"${1:type}" "${2:name}" { … }`, user typed `a`.
    let src = "resource \"a\" \"name\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    // Cursor sits between `a` and the closing quote of the first label.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 11)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let CompletionResponse::Array(items) = resp else {
        panic!("expected array response");
    };
    let item = items
        .iter()
        .find(|i| i.label == "aws_instance")
        .expect("aws_instance item missing");
    assert_eq!(
        item.insert_text.as_deref(),
        Some("aws_instance"),
        "expected bare type name, got: {:?}",
        item.insert_text
    );
    assert_eq!(
        item.insert_text_format,
        Some(lsp_types::InsertTextFormat::PLAIN_TEXT),
        "expected PLAIN_TEXT format"
    );
}

#[tokio::test]
async fn data_source_type_completion_in_closed_label_inserts_bare_name() {
    let u = uri("file:///a.tf");
    let src = "data \"a\" \"name\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    // `data ` is 5 chars, `"` at 5, `a` at 6, `"` at 7 — cursor at 7
    // sits between `a` and the closing quote.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 7)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let CompletionResponse::Array(items) = resp else {
        panic!("expected array response");
    };
    let item = items
        .iter()
        .find(|i| i.label == "aws_ami")
        .expect("aws_ami item missing");
    assert_eq!(
        item.insert_text.as_deref(),
        Some("aws_ami"),
        "expected bare type name, got: {:?}",
        item.insert_text
    );
}

// Regression guard: when the label is genuinely open (nothing to the
// right of the cursor), the full scaffold is still the right shape.
#[tokio::test]
async fn resource_type_completion_on_open_label_keeps_scaffold() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("resource \"", &u);
    install_aws_schema(&backend);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 10)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let CompletionResponse::Array(items) = resp else {
        panic!("expected array response");
    };
    let item = items
        .iter()
        .find(|i| i.label == "aws_instance")
        .expect("aws_instance item missing");
    let text = item.insert_text.as_deref().expect("insert_text set");
    assert!(
        text.starts_with("aws_instance\" \"${1:name}\" {"),
        "expected scaffold, got: {text:?}"
    );
    assert_eq!(
        item.insert_text_format,
        Some(lsp_types::InsertTextFormat::SNIPPET),
    );
}

#[tokio::test]
async fn resource_body_suggests_meta_arguments() {
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"web\" {\n\n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    for expected in [
        "count",
        "for_each",
        "provider",
        "depends_on",
        "lifecycle",
        "provisioner",
        "connection",
    ] {
        assert!(
            ls.contains(&expected.to_string()),
            "resource body completion missing {expected:?}; got: {ls:?}"
        );
    }
}

#[tokio::test]
async fn data_body_suggests_meta_arguments_minus_provisioner_connection() {
    let u = uri("file:///a.tf");
    let src = "data \"aws_ami\" \"x\" {\n\n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    for expected in ["count", "for_each", "provider", "depends_on", "lifecycle"] {
        assert!(
            ls.contains(&expected.to_string()),
            "data body completion missing {expected:?}; got: {ls:?}"
        );
    }
    for forbidden in ["provisioner", "connection"] {
        assert!(
            !ls.contains(&forbidden.to_string()),
            "data body should not offer {forbidden:?}; got: {ls:?}"
        );
    }
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

// Regression: cursor inside the *second* label of an existing
// `resource "TYPE" "NAME"` header must not receive resource-type
// scaffold snippets. Accepting such a snippet splices a whole new
// resource block into the already-open one and produces malformed
// code (see commit 9c26c79 "LSP snippet completions").
#[tokio::test]
async fn completion_in_resource_name_label_does_not_return_resource_types() {
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"web";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    // Cursor at the end of the (unclosed) name label.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, src.len() as u32)),
    )
    .await
    .expect("ok");

    // Accept either no completions or a set with no resource-type items.
    if let Some(response) = resp {
        match response {
            CompletionResponse::Array(items) => {
                for item in &items {
                    assert_ne!(
                        item.detail.as_deref(),
                        Some("resource type"),
                        "item {:?} was offered as a resource type inside the name label",
                        item.label
                    );
                    if let Some(text) = &item.insert_text {
                        assert!(
                            !text.contains("\" \"${1:name}\" {"),
                            "item {:?} carries a resource scaffold snippet: {text:?}",
                            item.label
                        );
                    }
                }
            }
            CompletionResponse::List(list) => {
                for item in &list.items {
                    assert_ne!(item.detail.as_deref(), Some("resource type"));
                }
            }
        }
    }
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
