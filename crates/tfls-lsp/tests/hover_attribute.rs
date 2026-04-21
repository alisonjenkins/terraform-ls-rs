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

// -------------------------------------------------------------------------
//  Meta-block attributes (lifecycle, provisioner, connection) aren't
//  in provider schemas. The hover shouldn't falsely report them as
//  "not in the schema for aws_foo" — it should render a language-
//  level description instead.
// -------------------------------------------------------------------------

#[tokio::test]
async fn hover_on_top_level_meta_arg_shows_description() {
    // Top-level meta-args (count/for_each/depends_on/provider) sit on
    // resource/data bodies but aren't in the provider schema. Hover
    // should surface the Terraform-language description, not
    // "attribute is not in the schema for aws_instance".
    let u = uri("file:///count.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  count = 3\n  ami = \"x\"\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(1, 3))
        .await
        .expect("some hover");
    assert!(
        md.contains("**meta-argument** `count`"),
        "expected meta-argument header; got: {md}"
    );
    assert!(
        md.contains("Creates that many instances"),
        "expected count description; got: {md}"
    );
    assert!(
        !md.contains("not in the schema"),
        "should NOT route to provider-schema-missing path; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_for_each_shows_description() {
    let u = uri("file:///for_each.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  for_each = toset([\"a\"])\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(1, 3))
        .await
        .expect("some hover");
    assert!(md.contains("**meta-argument** `for_each`"), "got: {md}");
    assert!(
        md.contains("Creates one instance of this resource per key")
            || md.contains("set(string)"),
        "expected for_each description; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_lifecycle_create_before_destroy_does_not_blame_provider() {
    let u = uri("file:///c.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  lifecycle {\n    create_before_destroy = true\n  }\n}\n";
    let b = backend_with(src, &u);
    // no schemas installed on purpose — we want to confirm the
    // hover renders even without the provider schema loaded.
    let md = hover_markdown(&b, &u, Position::new(2, 10))
        .await
        .expect("some hover");
    assert!(
        md.contains("create_before_destroy"),
        "expected attr name: {md}"
    );
    assert!(
        md.contains("lifecycle"),
        "expected path to mention lifecycle: {md}"
    );
    assert!(
        !md.contains("not in the schema"),
        "must NOT claim it's missing from the provider schema: {md}"
    );
    assert!(
        !md.contains("aws_instance"),
        "resource type shouldn't appear in a lifecycle-attr hover: {md}"
    );
}

#[tokio::test]
async fn hover_on_lifecycle_enabled_in_tf_file_warns_about_portability() {
    let u = uri("file:///c.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  lifecycle {\n    enabled = false\n  }\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(2, 5))
        .await
        .expect("some hover");
    assert!(md.contains("enabled"), "got: {md}");
    assert!(md.contains("OpenTofu"), "expected OpenTofu note: {md}");
    assert!(
        md.contains("Terraform"),
        "expected Terraform portability warning: {md}"
    );
    assert!(
        !md.contains("not in the schema"),
        "must NOT claim it's missing from provider schema: {md}"
    );
}

#[tokio::test]
async fn hover_on_lifecycle_enabled_in_tofu_file_is_silent_about_portability() {
    let u = uri("file:///c.tofu");
    let src = "resource \"aws_instance\" \"web\" {\n  lifecycle {\n    enabled = false\n  }\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(2, 5))
        .await
        .expect("some hover");
    assert!(md.contains("enabled"), "got: {md}");
    // Still mentions OpenTofu (it's documenting the feature) — but no
    // ⚠ portability warning.
    assert!(!md.contains("⚠"), "should be silent: {md}");
    assert!(
        !md.contains("Terraform does not support"),
        "should not warn about Terraform on a .tofu file: {md}"
    );
}

#[tokio::test]
async fn hover_on_terraform_backend_keyword_shows_builtin_docs() {
    // Cursor on `backend` inside `terraform { backend "s3" {} }`
    // should render the built-in backend docs (description + attr
    // summary), not fall through to the generic block-label hover.
    let u = uri("file:///backend.tf");
    let src = "terraform {\n  backend \"s3\" {}\n}\n";
    let b = backend_with(src, &u);
    // Cursor on the `b` of `backend` (line 1, col 2).
    let md = hover_markdown(&b, &u, Position::new(1, 4))
        .await
        .expect("some hover");
    assert!(
        md.contains("**block** `backend`"),
        "expected backend block header; got: {md}"
    );
    assert!(
        md.contains("on terraform block"),
        "expected terraform root annotation; got: {md}"
    );
    // s3 is the default label placeholder — schema attrs should show.
    assert!(
        md.contains("Remote state backend"),
        "expected backend detail; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_required_providers_keyword_shows_builtin_docs() {
    let u = uri("file:///required_providers.tf");
    let src = "terraform {\n  required_providers {}\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(1, 4))
        .await
        .expect("some hover");
    assert!(
        md.contains("**block** `required_providers`"),
        "expected required_providers block header; got: {md}"
    );
    assert!(
        md.contains("on terraform block"),
        "expected terraform annotation; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_cloud_workspaces_nested_block_shows_docs() {
    // Two-level descent: cursor on `workspaces` inside `terraform {
    // cloud { workspaces {} } }`.
    let u = uri("file:///cloud_ws.tf");
    let src = "terraform {\n  cloud {\n    workspaces {}\n  }\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(2, 6))
        .await
        .expect("some hover");
    assert!(
        md.contains("**block** `workspaces`"),
        "expected workspaces block header; got: {md}"
    );
    assert!(
        md.contains("inside `terraform.cloud`"),
        "expected parent-path annotation; got: {md}"
    );
    // Workspaces schema attrs should show (name / prefix / tags).
    assert!(
        md.contains("`name`") || md.contains("`prefix`") || md.contains("`tags`"),
        "expected workspaces attrs; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_terraform_required_version_attr_shows_builtin_docs() {
    // Attribute hover inside `terraform { required_version = … }` —
    // the built-in attr hover path.
    let u = uri("file:///rv.tf");
    let src = "terraform {\n  required_version = \">= 1.6\"\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(1, 4))
        .await
        .expect("some hover");
    assert!(
        md.contains("**attribute** `required_version`"),
        "expected attr header; got: {md}"
    );
    assert!(
        md.contains("in `terraform`"),
        "expected terraform path; got: {md}"
    );
    assert!(
        !md.contains("not in the schema"),
        "should NOT complain about missing provider schema; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_provider_alias_meta_attr_shows_description() {
    // `alias` isn't part of any provider's own config schema — it's
    // a language-level meta-attribute in PROVIDER_BLOCK_META_ATTRS.
    // Hover must NOT fall through to "attribute is not in the
    // schema for `aws`".
    let u = uri("file:///alias.tf");
    let src = "provider \"aws\" {\n  alias = \"east\"\n  region = \"us-east-1\"\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(1, 3))
        .await
        .expect("some hover");
    assert!(
        md.contains("**meta-argument** `alias`"),
        "expected meta-argument header; got: {md}"
    );
    assert!(
        md.contains("provider `aws`"),
        "expected provider label; got: {md}"
    );
    assert!(
        md.contains("Named alias"),
        "expected alias description; got: {md}"
    );
    assert!(
        !md.contains("not in the schema"),
        "must NOT route to provider-schema-missing path; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_variable_type_attr_shows_builtin_description() {
    let u = uri("file:///var_type.tf");
    let src = "variable \"foo\" {\n  type = string\n  default = \"x\"\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(1, 3))
        .await
        .expect("some hover");
    assert!(
        md.contains("**attribute** `type`"),
        "expected attr header; got: {md}"
    );
    assert!(
        md.contains("in `variable`"),
        "expected variable path; got: {md}"
    );
    assert!(
        md.contains("Type constraint"),
        "expected description from VARIABLE_BLOCK; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_module_source_attr_shows_builtin_description() {
    let u = uri("file:///mod.tf");
    let src = "module \"net\" {\n  source = \"./modules/net\"\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(1, 3))
        .await
        .expect("some hover");
    assert!(
        md.contains("**attribute** `source`"),
        "expected attr header; got: {md}"
    );
    assert!(
        md.contains("in `module`"),
        "expected module path; got: {md}"
    );
    assert!(
        md.contains("Module source"),
        "expected source description; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_output_value_attr_shows_builtin_description() {
    let u = uri("file:///out.tf");
    let src = "output \"foo\" {\n  value = \"x\"\n}\n";
    let b = backend_with(src, &u);
    let md = hover_markdown(&b, &u, Position::new(1, 3))
        .await
        .expect("some hover");
    assert!(
        md.contains("**attribute** `value`"),
        "expected attr header; got: {md}"
    );
    assert!(
        md.contains("in `output`"),
        "expected output path; got: {md}"
    );
    assert!(
        md.contains("Expression the output exports"),
        "expected value description; got: {md}"
    );
}

#[tokio::test]
async fn hover_on_nested_block_header_returns_block_docs_not_resource_label() {
    // Cursor on a nested block's identifier (e.g. the `r` of
    // `root_block_device`) should surface that block's schema
    // documentation — nesting mode, min/max_items, description,
    // attribute summary. Previously fell through to the enclosing
    // resource's symbol hover, which just said "resource aws_instance.x".
    let u = uri("file:///nested_block_hover.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  root_block_device {\n  }\n}\n";
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
                        "block": {
                            "attributes": {},
                            "block_types": {
                                "root_block_device": {
                                    "nesting_mode": "single",
                                    "max_items": 1,
                                    "block": {
                                        "description": "Customize details about the root block device of the instance.",
                                        "attributes": {
                                            "volume_size": { "type": "number", "optional": true }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "data_source_schemas": {}
            }
        }
    }"#,
    )
    .expect("parse schema");
    b.state.install_schemas(schema);

    // Cursor on `r` of `root_block_device` (line 1, col 2).
    let md = hover_markdown(&b, &u, Position::new(1, 2))
        .await
        .expect("some hover");

    assert!(
        md.contains("**block** `root_block_device`"),
        "expected nested-block header; got: {md}"
    );
    assert!(
        md.contains("aws_instance"),
        "expected enclosing resource type; got: {md}"
    );
    assert!(
        md.contains("nesting: single"),
        "expected nesting mode metadata; got: {md}"
    );
    assert!(
        md.contains("max_items: 1"),
        "expected cardinality metadata; got: {md}"
    );
    assert!(
        md.contains("Customize details about the root block device"),
        "expected block description; got: {md}"
    );
    assert!(
        md.contains("`volume_size`"),
        "expected attribute summary; got: {md}"
    );
    assert!(
        !md.starts_with("**resource**"),
        "must not fall through to resource-label hover; got: {md}"
    );
}
