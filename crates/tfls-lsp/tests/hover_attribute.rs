//! Tests for `attribute_hover` — specifically the fallback paths when
//! provider schemas aren't loaded.
//!
//! In practice `state.schemas` is populated by running
//! `terraform providers schema -json` in the workspace. If
//! `terraform init` hasn't been run or the CLI isn't on `$PATH`, the
//! lookup returns `None` and — before this test was added — the hover
//! call silently fell through to the enclosing resource label.
//!
//! The fallback is a user-visible hint explaining what to do about it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_schema::ProviderSchemas;
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
async fn hover_on_attribute_falls_back_when_no_schemas_loaded() {
    // No `install_schemas` — simulates a workspace where `terraform init`
    // has not been run (or the CLI was unavailable during fetch).
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  ami = \"ami-123\"\n}\n";
    let b = backend_with(src, &u);

    // Cursor on `ami` key.
    let md = hover_markdown(&b, &u, Position::new(1, 3))
        .await
        .expect("some hover");

    assert!(
        md.contains("attribute"),
        "expected attribute-level hover, got: {md}"
    );
    assert!(md.contains("ami"), "expected attribute name: {md}");
    assert!(md.contains("aws_instance"), "expected resource type: {md}");
    assert!(
        md.to_lowercase().contains("init"),
        "expected hint mentioning `terraform init`, got: {md}"
    );
    assert!(
        !md.starts_with("**resource**"),
        "must not fall through to resource-label hover: {md}"
    );
}

#[tokio::test]
async fn hover_on_attribute_falls_back_when_provider_missing() {
    // Install a schema for a DIFFERENT provider than the one referenced
    // in the source. The specific resource type isn't known, so we can't
    // produce attribute docs — but the user should still get a hint.
    let u = uri("file:///b.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  ami = \"ami-123\"\n}\n";
    let b = backend_with(src, &u);
    let schema: ProviderSchemas = sonic_rs::from_str(
        r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/hashicorp/null": {
                "provider": { "version": 0, "block": {} },
                "resource_schemas": {
                    "null_resource": { "version": 0, "block": {} }
                },
                "data_source_schemas": {}
            }
        }
    }"#,
    )
    .expect("parse schema");
    b.state.install_schemas(schema);

    let md = hover_markdown(&b, &u, Position::new(1, 3))
        .await
        .expect("some hover");

    assert!(md.contains("ami"), "expected attribute name: {md}");
    assert!(md.contains("aws_instance"), "expected resource type: {md}");
    assert!(
        md.to_lowercase().contains("provider"),
        "expected provider hint, got: {md}"
    );
}

#[tokio::test]
async fn hover_on_attribute_falls_back_when_attribute_not_in_schema() {
    // Provider + resource ARE known, but this specific attribute isn't
    // in the block's schema — e.g. user is typing a name that doesn't
    // exist on that resource. We still prefer attribute-level context
    // over the resource-label fallback.
    let u = uri("file:///c.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  totally_fake_attr = \"x\"\n}\n";
    let b = backend_with(src, &u);
    let schema: ProviderSchemas = sonic_rs::from_str(
        r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/hashicorp/aws": {
                "provider": { "version": 0, "block": {} },
                "resource_schemas": {
                    "aws_instance": {
                        "version": 1,
                        "block": { "attributes": { "ami": { "type": "string", "required": true } } }
                    }
                },
                "data_source_schemas": {}
            }
        }
    }"#,
    )
    .expect("parse schema");
    b.state.install_schemas(schema);

    let md = hover_markdown(&b, &u, Position::new(1, 5))
        .await
        .expect("some hover");

    assert!(
        md.contains("totally_fake_attr"),
        "expected attribute name to appear: {md}"
    );
    assert!(
        md.to_lowercase().contains("not") || md.to_lowercase().contains("unknown"),
        "expected a hint that the attribute is unknown, got: {md}"
    );
}
