//! Integration test exercising the navigation handlers end-to-end
//! through the [`Backend`] — no LSP wire protocol, just the handler
//! calls with fabricated params.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_schema::ProviderSchemas;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, HoverParams, PartialResultParams, Position,
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentPositionParams, Url,
    WorkDoneProgressParams,
};

fn uri(path: &str) -> Url {
    Url::parse(path).expect("valid url")
}

fn backend_with(src: &str, u: &Url) -> Backend {
    let (service, _socket) = LspService::new(Backend::new);
    let backend = service.inner();
    // Directly populate state; the handlers operate on StateStore, not raw RPC.
    backend
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));
    // LspService doesn't let us extract owned Backend, so clone Arc state into
    // a fresh Backend struct for the test. (In production, the service owns it.)
    Backend::with_shared_state(
        backend.client.clone(),
        backend.state.clone(),
        backend.jobs.clone(),
    )
}

#[tokio::test]
async fn goto_definition_finds_variable() {
    let u = uri("file:///test.tf");
    let src = "variable \"region\" { default = \"us-east-1\" }\noutput \"x\" { value = var.region }\n";
    let backend = backend_with(src, &u);

    // Cursor on "region" inside var.region (line 1, after `var.`).
    let pos = Position::new(1, 25);
    let result = tfls_lsp::handlers::navigation::goto_definition(
        &backend,
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok");

    let locations = match result {
        Some(GotoDefinitionResponse::Array(v)) => v,
        other => panic!("expected Array response, got {other:?}"),
    };
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].uri, u);
    // Variable is on line 0.
    assert_eq!(locations[0].range.start.line, 0);
}

#[tokio::test]
async fn references_includes_declaration_when_requested() {
    let u = uri("file:///refs.tf");
    let src = r#"variable "region" {}
output "a" { value = var.region }
output "b" { value = var.region }
"#;
    let backend = backend_with(src, &u);

    let pos = Position::new(1, 25); // cursor on first var.region
    let result = tfls_lsp::handlers::navigation::references(
        &backend,
        ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        },
    )
    .await
    .expect("ok");

    let locations = result.expect("locations present");
    // 1 declaration + 2 references.
    assert_eq!(locations.len(), 3);
}

#[tokio::test]
async fn hover_returns_kind_and_name() {
    let u = uri("file:///h.tf");
    let src = r#"variable "region" {}
output "x" { value = var.region }
"#;
    let backend = backend_with(src, &u);

    let pos = Position::new(1, 25);
    let hover = tfls_lsp::handlers::navigation::hover(
        &backend,
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok");

    let hover = hover.expect("some hover");
    let markdown = match hover.contents {
        tower_lsp::lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup, got {other:?}"),
    };
    assert!(markdown.contains("variable"), "got: {markdown}");
    assert!(markdown.contains("region"), "got: {markdown}");
}

#[tokio::test]
async fn hover_works_on_definition_label() {
    // Regression test: prior to the key_at_cursor refactor, hover would return
    // None when the cursor was on a block label. Now it should behave the same
    // as when the cursor is on a reference.
    let u = uri("file:///def.tf");
    let src = r#"variable "region" {}
"#;
    let backend = backend_with(src, &u);

    // Cursor on `region` inside `variable "region"` — column 12 puts us
    // inside the quoted label.
    let hover = tfls_lsp::handlers::navigation::hover(
        &backend,
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u.clone() },
                position: Position::new(0, 12),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some hover");

    let markdown = match hover.contents {
        tower_lsp::lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup, got {other:?}"),
    };
    assert!(markdown.contains("variable"), "got: {markdown}");
    assert!(markdown.contains("region"), "got: {markdown}");
}

#[tokio::test]
async fn hover_on_resource_attribute_returns_schema_description() {
    // Install a minimal schema so attribute hover has something to look up.
    let u = uri("file:///attr.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  ami = \"ami-123\"\n}\n";
    let backend = backend_with(src, &u);
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
                                "ami": { "type": "string", "required": true, "description": "The AMI ID to use for the instance." }
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
    backend.state.install_schemas(schema);

    // Cursor on `ami` key at line 1 column 3 — within `  ami = "ami-123"`.
    let hover = tfls_lsp::handlers::navigation::hover(
        &backend,
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u },
                position: Position::new(1, 3),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some hover");

    let markdown = match hover.contents {
        tower_lsp::lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup, got {other:?}"),
    };
    assert!(markdown.contains("attribute"), "got: {markdown}");
    assert!(markdown.contains("ami"), "got: {markdown}");
    assert!(markdown.contains("required"), "got: {markdown}");
    assert!(
        markdown.contains("The AMI ID to use"),
        "description missing from hover: {markdown}"
    );
}

#[tokio::test]
async fn hover_on_nested_block_attribute_resolves_through_block_types() {
    // Schema has a nested `root_block_device` block under `aws_instance`.
    let u = uri("file:///nested.tf");
    let src = "resource \"aws_instance\" \"web\" {\n  root_block_device {\n    volume_size = 100\n  }\n}\n";
    let backend = backend_with(src, &u);
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
                                    "nesting_mode": "list",
                                    "block": {
                                        "attributes": {
                                            "volume_size": {
                                                "type": "number",
                                                "optional": true,
                                                "description": "Size of the root volume in GiB."
                                            }
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
    backend.state.install_schemas(schema);

    // Cursor on `volume_size` at line 2 column 6.
    let hover = tfls_lsp::handlers::navigation::hover(
        &backend,
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u },
                position: Position::new(2, 6),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some hover");

    let markdown = match hover.contents {
        tower_lsp::lsp_types::HoverContents::Markup(m) => m.value,
        other => panic!("expected markup, got {other:?}"),
    };
    assert!(markdown.contains("volume_size"), "got: {markdown}");
    assert!(
        markdown.contains("root_block_device"),
        "nested path missing from hover: {markdown}"
    );
    assert!(
        markdown.contains("Size of the root volume"),
        "description missing from hover: {markdown}"
    );
}

#[tokio::test]
async fn goto_definition_on_nothing_returns_none() {
    let u = uri("file:///empty.tf");
    let backend = backend_with("", &u);

    let result = tfls_lsp::handlers::navigation::goto_definition(
        &backend,
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: u },
                position: Position::new(0, 0),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok");

    assert!(result.is_none());
}

// --- Module goto-def regressions ------------------------------------
//
// These tests set up a real on-disk directory layout so the full
// chain — module_sources → resolve_module_source →
// lookup_child_module_symbol — runs end-to-end, just like it would in
// a live editor session. Using a tempdir keeps the tests hermetic.

use std::path::Path;

fn file_uri(path: &Path) -> Url {
    Url::from_file_path(path).expect("file URL")
}

/// Register a .tf file on disk AND in the StateStore so the indexer
/// would have seen it. Returns the URI.
fn upsert_file(backend: &Backend, path: &Path, source: &str) -> Url {
    std::fs::write(path, source).expect("write .tf file");
    let u = file_uri(path);
    backend
        .state
        .upsert_document(DocumentState::new(u.clone(), source, 1));
    u
}

async fn goto_def_at(backend: &Backend, uri: &Url, pos: Position) -> Option<GotoDefinitionResponse> {
    tfls_lsp::handlers::navigation::goto_definition(
        backend,
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
}

fn single_location(resp: Option<GotoDefinitionResponse>) -> tower_lsp::lsp_types::Location {
    match resp {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        Some(GotoDefinitionResponse::Array(v)) if v.len() == 1 => v.into_iter().next().unwrap(),
        other => panic!("expected single location, got {other:?}"),
    }
}

#[tokio::test]
async fn goto_def_on_module_input_attr_jumps_to_variable_decl() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let child_dir = root.join("child");
    std::fs::create_dir(&child_dir).unwrap();

    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );

    // Child module declares a `region` variable.
    let child_u = upsert_file(
        &backend,
        &child_dir.join("variables.tf"),
        "variable \"region\" { type = string }\n",
    );

    // Caller references the child module and sets `region = "eu"`.
    let caller_path = root.join("main.tf");
    let caller_src = "module \"net\" {\n  source = \"./child\"\n  region = \"eu\"\n}\n";
    let caller_u = upsert_file(&backend, &caller_path, caller_src);

    // Line 2, col 4 → on the `r` of `region = "eu"`.
    let loc = single_location(goto_def_at(&backend, &caller_u, Position::new(2, 4)).await);
    assert_eq!(loc.uri, child_u, "should land in child's variables.tf");
    assert_eq!(loc.range.start.line, 0, "variable is on line 0");
}

#[tokio::test]
async fn goto_def_on_module_output_segment_jumps_to_output_decl() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let child_dir = root.join("child");
    std::fs::create_dir(&child_dir).unwrap();

    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );

    let child_u = upsert_file(
        &backend,
        &child_dir.join("outputs.tf"),
        "output \"subnet_id\" { value = \"\" }\n",
    );

    let caller_path = root.join("main.tf");
    let caller_src =
        "module \"net\" {\n  source = \"./child\"\n}\n\noutput \"x\" { value = module.net.subnet_id }\n";
    let caller_u = upsert_file(&backend, &caller_path, caller_src);

    // Line 4, col 36 → cursor on `s` of `subnet_id` in
    // `output "x" { value = module.net.subnet_id }`.
    let loc = single_location(goto_def_at(&backend, &caller_u, Position::new(4, 36)).await);
    assert_eq!(loc.uri, child_u, "should land in child's outputs.tf");
    assert_eq!(loc.range.start.line, 0, "output is on line 0");
}

#[tokio::test]
async fn goto_def_on_module_label_still_jumps_to_module_block() {
    // Cursor on the `net` segment of `module.net.subnet_id` must keep
    // resolving to the `module "net" { }` call header in the SAME
    // file — NOT into the child module. The user is navigating on
    // the module name, not on a value inside it.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let child_dir = root.join("child");
    std::fs::create_dir(&child_dir).unwrap();

    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );

    upsert_file(
        &backend,
        &child_dir.join("outputs.tf"),
        "output \"subnet_id\" { value = \"\" }\n",
    );

    let caller_path = root.join("main.tf");
    let caller_src =
        "module \"net\" {\n  source = \"./child\"\n}\n\noutput \"x\" { value = module.net.subnet_id }\n";
    let caller_u = upsert_file(&backend, &caller_path, caller_src);

    // Line 4, col 29 → on `n` of `net` in `module.net.subnet_id`.
    let loc = single_location(goto_def_at(&backend, &caller_u, Position::new(4, 29)).await);
    assert_eq!(loc.uri, caller_u, "label goto-def stays in the caller");
    assert_eq!(
        loc.range.start.line, 0,
        "should point at `module \"net\"` call header on line 0"
    );
}

#[tokio::test]
async fn goto_def_on_unknown_module_input_returns_none() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let child_dir = root.join("child");
    std::fs::create_dir(&child_dir).unwrap();

    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );

    // Child declares `region` — the caller sets a typo `regin`.
    upsert_file(
        &backend,
        &child_dir.join("variables.tf"),
        "variable \"region\" {}\n",
    );

    let caller_path = root.join("main.tf");
    let caller_src = "module \"net\" {\n  source = \"./child\"\n  regin = \"eu\"\n}\n";
    let caller_u = upsert_file(&backend, &caller_path, caller_src);

    // Cursor on `regin` — unknown in the child. Goto-def must return
    // None (don't fall through to something bogus like jumping to the
    // module's own call header, which would be confusing).
    let result = goto_def_at(&backend, &caller_u, Position::new(2, 4)).await;
    assert!(result.is_none(), "unknown input should yield None, got {result:?}");
}

#[tokio::test]
async fn goto_def_on_module_input_resolved_via_modules_json() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();

    // Simulate a registry/git module: the actual code lives at
    // `root/cached/net`, advertised via `.terraform/modules/modules.json`
    // under the key `net`.
    let cached = root.join("cached").join("net");
    std::fs::create_dir_all(&cached).unwrap();
    std::fs::create_dir_all(root.join(".terraform").join("modules")).unwrap();
    std::fs::write(
        root.join(".terraform").join("modules").join("modules.json"),
        r#"{"Modules":[{"Key":"net","Source":"hashicorp/net/aws","Dir":"cached/net"}]}"#,
    )
    .unwrap();

    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );

    let child_u = upsert_file(
        &backend,
        &cached.join("variables.tf"),
        "variable \"region\" { type = string }\n",
    );

    let caller_path = root.join("main.tf");
    let caller_src =
        "module \"net\" {\n  source = \"hashicorp/net/aws\"\n  region = \"eu\"\n}\n";
    let caller_u = upsert_file(&backend, &caller_path, caller_src);

    let loc = single_location(goto_def_at(&backend, &caller_u, Position::new(2, 4)).await);
    assert_eq!(loc.uri, child_u, "should resolve through modules.json");
    assert_eq!(loc.range.start.line, 0);
}
