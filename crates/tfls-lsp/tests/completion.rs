//! Integration tests for the completion handler.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_schema::ProviderSchemas;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    CompletionContext, CompletionItem, CompletionParams, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, PartialResultParams, Position, TextDocumentIdentifier,
    TextDocumentPositionParams, Url, WorkDoneProgressParams,
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
                            },
                            "block_types": {
                                "root_block_device": {
                                    "nesting_mode": "single",
                                    "block": {
                                        "attributes": {
                                            "volume_size": { "type": "number", "optional": true }
                                        }
                                    }
                                },
                                "ebs_block_device": {
                                    "nesting_mode": "list",
                                    "block": {
                                        "attributes": {
                                            "device_name": { "type": "string", "required": true }
                                        }
                                    }
                                }
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

// The `resource` / `data` top-level items intentionally stop at the
// opening quote of the type label so that the per-type scaffold
// completion (which inserts required attrs + a `${1:name}` placeholder)
// can take over cleanly once the user types a character.
#[tokio::test]
async fn top_level_resource_data_chain_into_type_scaffold() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("", &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let CompletionResponse::Array(items) = resp else {
        panic!("expected array");
    };
    for (label, expected) in [("resource", "resource \""), ("data", "data \"")] {
        let item = items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("missing {label}"));
        assert_eq!(
            item.insert_text.as_deref(),
            Some(expected),
            "{label} top-level snippet must end at open quote"
        );
    }
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
async fn resource_body_excludes_pure_computed_attributes() {
    // Regression: attributes with `computed = true` and neither
    // `required` nor `optional` are pure provider outputs and cannot
    // be assigned. They must not appear in body completion. The
    // `optional && computed` case ("computed-optional") is writable
    // and must stay in.
    let u = uri("file:///a.tf");
    let src = "resource \"widget\" \"x\" {\n\n}\n";
    let backend = fresh_backend(src, &u);
    let schema: ProviderSchemas = sonic_rs::from_str(
        r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/acme/widget": {
                "provider": { "version": 0, "block": {} },
                "resource_schemas": {
                    "widget": {
                        "version": 1,
                        "block": {
                            "attributes": {
                                "name": { "type": "string", "required": true },
                                "id":   { "type": "string", "computed": true },
                                "tags": { "type": ["map", "string"], "optional": true, "computed": true }
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

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"name".to_string()), "required attr should appear");
    assert!(ls.contains(&"tags".to_string()), "computed-optional attr should appear");
    assert!(
        !ls.contains(&"id".to_string()),
        "pure computed attr must not appear; labels were {ls:?}"
    );
}

#[tokio::test]
async fn resource_body_inside_nested_block_suggests_nested_attrs() {
    // Cursor inside `root_block_device { … }` should surface that
    // nested block's own attributes (e.g. `volume_size`) and suppress
    // the outer `aws_instance` attributes + all meta-arguments.
    let u = uri("file:///nested.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  root_block_device {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    // Line 2 (0-based), column 4 → inside the nested block body on the
    // indented blank line.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(
        ls.contains(&"volume_size".to_string()),
        "nested attr missing; got {ls:?}"
    );
    assert!(
        !ls.contains(&"ami".to_string()),
        "outer resource attr leaked; got {ls:?}"
    );
    assert!(
        !ls.contains(&"instance_type".to_string()),
        "outer resource attr leaked; got {ls:?}"
    );
    assert!(
        !ls.contains(&"count".to_string()),
        "meta-argument leaked into nested block; got {ls:?}"
    );
    assert!(
        !ls.contains(&"for_each".to_string()),
        "meta-argument leaked into nested block; got {ls:?}"
    );
    assert!(
        !ls.contains(&"lifecycle".to_string()),
        "meta block leaked into nested block; got {ls:?}"
    );
}

#[tokio::test]
async fn nested_block_completion_prefills_required_attrs() {
    // When the user completes a nested block whose schema has required
    // attrs, the inserted snippet should pre-fill each required attr
    // on its own line with a type-aware placeholder — mirroring the
    // top-level resource scaffold. `ebs_block_device` has
    // `device_name` (string, required); the snippet must include it.
    let u = uri("file:///nested_required.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");

    let items: Vec<CompletionItem> = match resp {
        CompletionResponse::Array(a) => a,
        CompletionResponse::List(l) => l.items,
    };
    let item = items
        .iter()
        .find(|i| i.label == "ebs_block_device")
        .expect("ebs_block_device suggestion present");
    let text = item.insert_text.as_deref().expect("insert_text set");
    assert!(
        text.contains("device_name = \"${1}\""),
        "required string attr should be pre-filled with type-aware placeholder; got {text:?}"
    );
    assert!(
        text.starts_with("ebs_block_device {\n"),
        "snippet should open the block; got {text:?}"
    );
    assert!(
        text.trim_end().ends_with('}'),
        "snippet should close the block; got {text:?}"
    );

    // `root_block_device` has only optional attrs — no prefill, empty
    // body with `$0` tabstop.
    let root_item = items
        .iter()
        .find(|i| i.label == "root_block_device")
        .expect("root_block_device suggestion present");
    let root_text = root_item.insert_text.as_deref().expect("insert_text set");
    assert_eq!(root_text, "root_block_device {\n  $0\n}");
}

#[tokio::test]
async fn cursor_on_nested_block_header_suggests_parent_body_not_child_attrs() {
    // Regression: when the cursor sits on the identifier of an existing
    // nested block header (e.g. the `r` of `root_block_device {`), the
    // AST-based descent previously classified us as "inside the child
    // body". Its present_attrs then listed every attr of the nested
    // block as already set — filtering all suggestions to zero. The
    // fix: only descend into a block when the cursor is past its
    // opening `{`.
    let u = uri("file:///cursor_on_header.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  root_block_device {\n    volume_size = 8\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    // Line 1, column 2 — the `r` of `root_block_device` on its header
    // line. That's inside the outer resource body, before the nested
    // block's `{`. We should surface the resource-body options, not an
    // empty list.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    // `ami` is a required attr of aws_instance — should appear.
    assert!(
        ls.contains(&"ami".to_string()),
        "resource-body attr missing (header-cursor regression); got {ls:?}"
    );
    // `volume_size` belongs to the nested block's schema, not the
    // outer resource. Must not leak into the outer body.
    assert!(
        !ls.contains(&"volume_size".to_string()),
        "nested-block attr leaked to parent body; got {ls:?}"
    );
    // Likewise, `root_block_device` itself should be treated as already
    // present in the resource body (the header we're cursoring on *is*
    // that block) — don't suggest it again as a block to add.
    assert!(
        !ls.contains(&"root_block_device".to_string()),
        "existing nested block re-suggested; got {ls:?}"
    );
}

#[tokio::test]
async fn resource_body_inside_nested_block_excludes_pure_computed() {
    // Same writability filter we apply at the top level should also
    // apply inside a nested block.
    let u = uri("file:///nested_computed.tf");
    let src = "resource \"widget\" \"x\" {\n  shell {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let schema: ProviderSchemas = sonic_rs::from_str(
        r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/acme/widget": {
                "provider": { "version": 0, "block": {} },
                "resource_schemas": {
                    "widget": {
                        "version": 1,
                        "block": {
                            "attributes": {},
                            "block_types": {
                                "shell": {
                                    "nesting_mode": "single",
                                    "block": {
                                        "attributes": {
                                            "command": { "type": "string", "required": true },
                                            "exit_code": { "type": "number", "computed": true },
                                            "env": { "type": ["map", "string"], "optional": true, "computed": true }
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

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"command".to_string()), "required nested attr missing");
    assert!(ls.contains(&"env".to_string()), "computed-optional nested attr missing");
    assert!(
        !ls.contains(&"exit_code".to_string()),
        "pure-computed nested attr leaked; got {ls:?}"
    );
}

#[tokio::test]
async fn resource_body_inside_doubly_nested_block_suggests_innermost_attrs() {
    // Two-level descent: cursor inside `outer { inner { … } }` should
    // surface `inner`'s attrs only — neither `outer`'s nor the
    // resource root's.
    let u = uri("file:///doubly.tf");
    let src = "resource \"widget\" \"x\" {\n  outer {\n    inner {\n      \n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let schema: ProviderSchemas = sonic_rs::from_str(
        r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/acme/widget": {
                "provider": { "version": 0, "block": {} },
                "resource_schemas": {
                    "widget": {
                        "version": 1,
                        "block": {
                            "attributes": {
                                "root_attr": { "type": "string", "optional": true }
                            },
                            "block_types": {
                                "outer": {
                                    "nesting_mode": "single",
                                    "block": {
                                        "attributes": {
                                            "outer_attr": { "type": "string", "optional": true }
                                        },
                                        "block_types": {
                                            "inner": {
                                                "nesting_mode": "single",
                                                "block": {
                                                    "attributes": {
                                                        "leaf_attr": { "type": "string", "optional": true }
                                                    }
                                                }
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

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 6)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"leaf_attr".to_string()), "innermost attr missing; got {ls:?}");
    assert!(!ls.contains(&"outer_attr".to_string()), "outer block attr leaked; got {ls:?}");
    assert!(!ls.contains(&"root_attr".to_string()), "root attr leaked; got {ls:?}");
}

#[tokio::test]
async fn data_body_inside_nested_block_suggests_nested_attrs() {
    // Same machinery must work for `data` blocks, not just `resource`.
    let u = uri("file:///data_nested.tf");
    let src = "data \"aws_bundle\" \"x\" {\n  filter {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let schema: ProviderSchemas = sonic_rs::from_str(
        r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/hashicorp/aws": {
                "provider": { "version": 0, "block": {} },
                "resource_schemas": {},
                "data_source_schemas": {
                    "aws_bundle": {
                        "version": 0,
                        "block": {
                            "attributes": {},
                            "block_types": {
                                "filter": {
                                    "nesting_mode": "list",
                                    "block": {
                                        "attributes": {
                                            "name":   { "type": "string", "required": true },
                                            "values": { "type": ["list", "string"], "required": true }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }"#,
    )
    .expect("parse schema");
    backend.state.install_schemas(schema);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"name".to_string()), "nested attr missing; got {ls:?}");
    assert!(ls.contains(&"values".to_string()), "nested attr missing; got {ls:?}");
    assert!(
        !ls.contains(&"count".to_string()),
        "meta-argument leaked into nested data block; got {ls:?}"
    );
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
    // With required attrs, the block closes immediately after the last
    // attr — no trailing `  $0\n` line that would render as a blank.
    assert!(
        text.ends_with("= \"${2}\"\n}"),
        "scaffold should end right after last required attr, got: {text:?}"
    );
    assert!(
        !text.contains("$0"),
        "no $0 expected when required attrs are present, got: {text:?}"
    );
}

// No required attrs → scaffold keeps `$0` so the cursor lands inside
// the empty body for free-form editing.
#[tokio::test]
async fn resource_type_scaffold_keeps_dollar_zero_when_no_required_attrs() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("resource \"", &u);
    // Install a schema whose only resource type has no required attrs.
    let schema: ProviderSchemas = sonic_rs::from_str(
        r#"{
            "format_version": "1.0",
            "provider_schemas": {
                "registry.terraform.io/hashicorp/aws": {
                    "provider": { "version": 0, "block": {} },
                    "resource_schemas": {
                        "aws_no_required": {
                            "version": 1,
                            "block": {
                                "attributes": {
                                    "optional_attr": { "type": "string", "optional": true }
                                }
                            }
                        }
                    }
                }
            }
        }"#,
    )
    .expect("parse schema");
    backend.state.install_schemas(schema);

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
        .find(|i| i.label == "aws_no_required")
        .expect("aws_no_required item missing");
    let text = item.insert_text.as_deref().expect("insert_text set");
    assert!(
        text.contains("  $0\n}"),
        "empty-body scaffold should still carry `$0`, got: {text:?}"
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

// --- Body-filter regressions --------------------------------------
//
// Inside a `resource`/`data` body, suggestions should not re-offer
// attributes or singleton blocks that are already set. Repeatable
// nested blocks (schema `list`/`set`/`map`/`group` + `provisioner`)
// should still be offered even when one already exists.

#[tokio::test]
async fn resource_body_filters_already_set_schema_attribute() {
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  ami = \"ami-1\"\n  \n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    // Cursor on the empty line inside the body (line 2, col 2).
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(!ls.contains(&"ami".to_string()), "ami already set; got: {ls:?}");
    assert!(ls.contains(&"instance_type".to_string()), "got: {ls:?}");
    assert!(ls.contains(&"tags".to_string()), "got: {ls:?}");
}

#[tokio::test]
async fn resource_body_filters_already_set_meta_argument() {
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  count = 2\n  \n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(!ls.contains(&"count".to_string()), "count already set; got: {ls:?}");
    for still_offered in ["for_each", "provider", "depends_on"] {
        assert!(
            ls.contains(&still_offered.to_string()),
            "{still_offered} must still be offered; got: {ls:?}"
        );
    }
}

#[tokio::test]
async fn resource_body_filters_singleton_meta_block() {
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  lifecycle {\n    create_before_destroy = true\n  }\n  \n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    // Cursor on the empty body line after the lifecycle block closes.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(4, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(
        !ls.contains(&"lifecycle".to_string()),
        "lifecycle is singleton; got: {ls:?}"
    );
    assert!(ls.contains(&"provisioner".to_string()), "got: {ls:?}");
    assert!(ls.contains(&"connection".to_string()), "got: {ls:?}");
}

#[tokio::test]
async fn resource_body_keeps_repeatable_meta_block_after_first() {
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  provisioner \"local-exec\" {\n    command = \"echo\"\n  }\n  \n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(4, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(
        ls.contains(&"provisioner".to_string()),
        "provisioner is repeatable; should still appear; got: {ls:?}"
    );
}

#[tokio::test]
async fn resource_body_filters_schema_single_and_keeps_list_nested_block() {
    let u = uri("file:///a.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  root_block_device {\n    volume_size = 20\n  }\n  ebs_block_device {\n    device_name = \"/dev/sda1\"\n  }\n  \n}\n";
    let backend = fresh_backend(src, &u);
    install_aws_schema(&backend);

    // Cursor on the empty line after both nested blocks (line 7).
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(7, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(
        !ls.contains(&"root_block_device".to_string()),
        "root_block_device is schema-single; got: {ls:?}"
    );
    assert!(
        ls.contains(&"ebs_block_device".to_string()),
        "ebs_block_device is schema-list and must still be offered; got: {ls:?}"
    );
}

// --- Reference-completion regressions -----------------------------
//
// `TYPE.`, `TYPE.NAME.`, `data.TYPE.`, `data.TYPE.NAME.`, and
// `var.NAME.field.` should each return a focused, module-scoped menu.

fn insert_doc(backend: &Backend, u: &Url, src: &str) {
    backend
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));
}

/// Extract `"|"` cursor marker from `src`, returning the clean source
/// and the LSP position of the cursor. The marker never appears in a
/// real Terraform file so we can use it to pin cursor positions inside
/// parseable sources.
fn src_with_cursor(marked: &str) -> (String, Position) {
    const MARKER: &str = "|";
    let idx = marked
        .find(MARKER)
        .unwrap_or_else(|| panic!("missing cursor `|` in test source"));
    let cleaned = format!("{}{}", &marked[..idx], &marked[idx + MARKER.len()..]);
    let before = &marked[..idx];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = (idx - line_start) as u32;
    (cleaned, Position::new(line, col))
}

#[tokio::test]
async fn resource_ref_suggests_only_names_of_matching_type() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "resource \"aws_iam_role\" \"role1\" {}\nresource \"aws_instance\" \"web\" {}\noutput \"x\" { value = aws_iam_role.|xxx }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["role1".to_string()]);
}

#[tokio::test]
async fn data_source_ref_suggests_only_names_of_matching_type() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "data \"aws_ami\" \"ubuntu\" { owners = [\"x\"] }\noutput \"x\" { value = data.aws_ami.|xxx }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["ubuntu".to_string()]);
}

#[tokio::test]
async fn resource_attr_suggests_schema_attributes() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "resource \"aws_instance\" \"web\" {}\noutput \"x\" { value = aws_instance.web.|xxx }\n",
    );
    let backend = fresh_backend(&src, &u);
    install_aws_schema(&backend);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"ami".to_string()), "got: {ls:?}");
    assert!(ls.contains(&"instance_type".to_string()), "got: {ls:?}");
    assert!(ls.contains(&"tags".to_string()), "got: {ls:?}");
}

#[tokio::test]
async fn data_source_attr_has_focused_response() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "data \"aws_ami\" \"ubuntu\" {}\noutput \"x\" { value = data.aws_ami.ubuntu.|xxx }\n",
    );
    let backend = fresh_backend(&src, &u);
    install_aws_schema(&backend);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok");
    // With the current fixture `aws_ami` has an empty block; the menu
    // is either None or an empty list. Either way, the classifier
    // reached `DataSourceAttr` (covered by the tfls-core unit test) and
    // didn't leak unrelated variable/local names.
    if let Some(CompletionResponse::Array(items)) = resp {
        for it in &items {
            assert_ne!(it.label, "ubuntu".to_string(), "data-source name leaked as attr");
        }
    }
}

#[tokio::test]
async fn var_ref_is_module_scoped_across_files() {
    let ua = uri("file:///mod/a.tf");
    let ub = uri("file:///mod/b.tf");
    let uc = uri("file:///other/c.tf");
    let (src_a, pos) = src_with_cursor(
        "variable \"a\" {}\noutput \"x\" { value = var.|xxx }\n",
    );
    let backend = fresh_backend(&src_a, &ua);
    insert_doc(&backend, &ub, "variable \"b\" {}\n");
    insert_doc(&backend, &uc, "variable \"c\" {}\n");
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&ua, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"a".to_string()), "got: {ls:?}");
    assert!(ls.contains(&"b".to_string()), "got: {ls:?}");
    assert!(!ls.contains(&"c".to_string()), "`c` lives in /other; got: {ls:?}");
}

#[tokio::test]
async fn local_ref_is_module_scoped_across_files() {
    let ua = uri("file:///mod/a.tf");
    let ub = uri("file:///mod/b.tf");
    let uc = uri("file:///other/c.tf");
    let (src_a, pos) = src_with_cursor(
        "locals { a = 1 }\noutput \"x\" { value = local.|xxx }\n",
    );
    let backend = fresh_backend(&src_a, &ua);
    insert_doc(&backend, &ub, "locals { b = 2 }\n");
    insert_doc(&backend, &uc, "locals { c = 3 }\n");
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&ua, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"a".to_string()), "got: {ls:?}");
    assert!(ls.contains(&"b".to_string()), "got: {ls:?}");
    assert!(!ls.contains(&"c".to_string()), "got: {ls:?}");
}

#[tokio::test]
async fn resource_ref_is_module_scoped_across_files() {
    let ua = uri("file:///mod/a.tf");
    let ub = uri("file:///mod/b.tf");
    let uc = uri("file:///other/c.tf");
    let (src_a, pos) = src_with_cursor(
        "resource \"aws_iam_role\" \"admin\" {}\noutput \"x\" { value = aws_iam_role.|xxx }\n",
    );
    let backend = fresh_backend(&src_a, &ua);
    insert_doc(&backend, &ub, "resource \"aws_iam_role\" \"reader\" {}\n");
    insert_doc(&backend, &uc, "resource \"aws_iam_role\" \"outsider\" {}\n");
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&ua, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"admin".to_string()), "got: {ls:?}");
    assert!(ls.contains(&"reader".to_string()), "got: {ls:?}");
    assert!(
        !ls.contains(&"outsider".to_string()),
        "outsider lives in /other; got: {ls:?}"
    );
}

#[tokio::test]
async fn variable_attr_ref_suggests_object_fields() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "variable \"config\" {\n  type = object({ region = string, enabled = bool })\n}\noutput \"x\" { value = var.config.|xxx }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["enabled".to_string(), "region".to_string()]);
}

#[tokio::test]
async fn variable_attr_ref_drills_into_nested_object() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "variable \"c\" {\n  type = object({ inner = object({ a = string, b = number }) })\n}\noutput \"x\" { value = var.c.inner.|xxx }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["a".to_string(), "b".to_string()]);
}

// --- Bracket-index / map-key drill-in regressions ---------------

#[tokio::test]
async fn resource_index_key_suggests_for_each_keys() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "resource \"aws_vpc\" \"eu\" { for_each = toset([\"vpc\", \"dev\"]) }\noutput \"x\" { value = aws_vpc.eu[|xxx] }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["dev".to_string(), "vpc".to_string()]);
}

#[tokio::test]
async fn variable_index_key_suggests_default_map_keys() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "variable \"regions\" { default = { \"eu-west-1\" = {}, \"us-east-1\" = {} } }\noutput \"x\" { value = var.regions[|xxx] }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["eu-west-1".to_string(), "us-east-1".to_string()]);
}

#[tokio::test]
async fn variable_index_key_suggests_object_type_fields_union_with_default() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "variable \"cfg\" { type = object({ a = string, b = number }) }\noutput \"x\" { value = var.cfg[|xxx] }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["a".to_string(), "b".to_string()]);
}

#[tokio::test]
async fn local_index_key_suggests_map_keys() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "locals { cfg = { foo = 1, bar = 2 } }\noutput \"x\" { value = local.cfg[|xxx] }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["bar".to_string(), "foo".to_string()]);
}

#[tokio::test]
async fn module_index_key_suggests_for_each_keys() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "module \"web\" {\n  source = \"./mod\"\n  for_each = toset([\"a\", \"b\"])\n}\noutput \"x\" { value = module.web[|xxx] }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["a".to_string(), "b".to_string()]);
}

#[tokio::test]
async fn deep_drill_in_via_brackets() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        r#"variable "regions" {
  default = {
    "eu-west-1" = {
      "subnet_cidrs" = {
        "eu-west-1a" = "10.0.1.0/24"
        "eu-west-1b" = "10.0.2.0/24"
      }
    }
  }
}
output "x" { value = var.regions["eu-west-1"]["subnet_cidrs"][|xxx] }
"#,
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["eu-west-1a".to_string(), "eu-west-1b".to_string()]);
}

#[tokio::test]
async fn index_key_item_emits_text_edit_with_closing_bracket() {
    let u = uri("file:///mod/a.tf");
    // Uses a parseable source; cursor sits right after `[`, with an
    // existing `]` on the same line (the `xxx]` placeholder).
    let (src, pos) = src_with_cursor(
        "resource \"aws_vpc\" \"eu\" { for_each = toset([\"vpc\"]) }\noutput \"x\" { value = aws_vpc.eu[|xxx] }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let CompletionResponse::Array(items) = resp else {
        panic!("expected array");
    };
    let item = items.iter().find(|i| i.label == "vpc").expect("vpc");
    let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
        panic!("expected text edit, got {:?}", item.text_edit);
    };
    assert_eq!(edit.new_text, "\"vpc\"]", "always emits closing bracket");
    // Range covers the trailing `xxx]` placeholder, so the existing
    // `]` is replaced rather than duplicated.
    assert!(
        edit.range.end.character > edit.range.start.character,
        "range must extend over existing partial + close to avoid duplication"
    );
}

#[tokio::test]
async fn index_key_item_replaces_partial_quoted_key() {
    let u = uri("file:///mod/a.tf");
    // Partial key already typed: `aws_vpc.eu["vp|"]`.
    let (src, pos) = src_with_cursor(
        "resource \"aws_vpc\" \"eu\" { for_each = toset([\"vpc\"]) }\noutput \"x\" { value = aws_vpc.eu[\"vp|\"] }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let CompletionResponse::Array(items) = resp else {
        panic!("expected array");
    };
    let item = items.iter().find(|i| i.label == "vpc").expect("vpc");
    let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
        panic!("expected text edit");
    };
    assert_eq!(edit.new_text, "\"vpc\"]");
}

#[tokio::test]
async fn resource_attr_after_bracket_index_suggests_schema_attrs_without_equals() {
    // Regression: typing `aws_instance.web["vpc"].` used to fall
    // through to resource-body completion (inside the enclosing
    // block), which inserts `name = ${1}`. It should classify as
    // ResourceAttr and offer plain attribute names.
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "resource \"aws_instance\" \"x\" {\n  ami = aws_instance.web[\"vpc\"].|xxx\n}\n",
    );
    let backend = fresh_backend(&src, &u);
    install_aws_schema(&backend);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok")
        .expect("some completions");
    let CompletionResponse::Array(items) = resp else {
        panic!("expected array");
    };
    for it in &items {
        if let Some(text) = &it.insert_text {
            assert!(
                !text.contains(" = "),
                "attribute ref completion must not carry ` = `; item {:?} inserts {text:?}",
                it.label
            );
        }
    }
    let labels: Vec<_> = items.iter().map(|i| i.label.clone()).collect();
    assert!(labels.contains(&"ami".to_string()), "got: {labels:?}");
    // Should NOT include meta-args like `count` (those are body-only).
    assert!(
        !labels.contains(&"count".to_string()),
        "meta-arg `count` leaked into attribute-ref menu: {labels:?}"
    );
}

#[tokio::test]
async fn index_key_empty_when_for_each_dynamic() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "resource \"aws_vpc\" \"eu\" { for_each = var.m }\noutput \"x\" { value = aws_vpc.eu[|xxx] }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok");
    assert!(resp.is_none(), "dynamic for_each shouldn't offer keys; got {resp:?}");
}

#[tokio::test]
async fn variable_attr_ref_returns_empty_for_non_object() {
    let u = uri("file:///mod/a.tf");
    let (src, pos) = src_with_cursor(
        "variable \"region\" { type = string }\noutput \"x\" { value = var.region.|xxx }\n",
    );
    let backend = fresh_backend(&src, &u);
    let resp = tfls_lsp::handlers::completion::completion(&backend, make_params(&u, pos))
        .await
        .expect("ok");
    assert!(
        resp.is_none(),
        "primitive var shouldn't expose fields; got: {resp:?}"
    );
}

// --- Built-in block completion (terraform / variable / output / module /
// provider / backend / required_providers) -------------------------------

#[tokio::test]
async fn terraform_block_body_suggests_required_version_and_blocks() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let items = match resp {
        CompletionResponse::Array(v) => v,
        CompletionResponse::List(l) => l.items,
    };
    let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
    assert!(labels.contains(&"required_version".to_string()));
    assert!(labels.contains(&"required_providers".to_string()));
    assert!(labels.contains(&"backend".to_string()));
    assert!(labels.contains(&"cloud".to_string()));
    // Must NOT offer resource/data-specific meta-args.
    assert!(!labels.contains(&"count".to_string()));
    assert!(!labels.contains(&"for_each".to_string()));

    // Labeled blocks MUST expand to a snippet that includes a type
    // label placeholder — `backend "s3" { ... }`, not `backend { ... }`.
    let backend_item = items.iter().find(|i| i.label == "backend").unwrap();
    let backend_insert = backend_item.insert_text.as_deref().unwrap_or("");
    assert!(
        backend_insert.starts_with("backend \""),
        "backend snippet must open a label; got {backend_insert:?}"
    );
    assert!(
        backend_insert.contains("${1:"),
        "backend snippet must place a tabstop on the label; got {backend_insert:?}"
    );
    let provider_meta_item = items.iter().find(|i| i.label == "provider_meta").unwrap();
    let pm_insert = provider_meta_item.insert_text.as_deref().unwrap_or("");
    assert!(
        pm_insert.starts_with("provider_meta \""),
        "provider_meta snippet must open a label; got {pm_insert:?}"
    );

    // Unlabeled blocks must NOT include a stray label placeholder.
    let rp_item = items.iter().find(|i| i.label == "required_providers").unwrap();
    let rp_insert = rp_item.insert_text.as_deref().unwrap_or("");
    assert!(
        rp_insert.starts_with("required_providers {"),
        "required_providers must be unlabeled; got {rp_insert:?}"
    );
    let cloud_item = items.iter().find(|i| i.label == "cloud").unwrap();
    let cloud_insert = cloud_item.insert_text.as_deref().unwrap_or("");
    assert!(
        cloud_insert.starts_with("cloud {"),
        "cloud must be unlabeled; got {cloud_insert:?}"
    );
}

#[tokio::test]
async fn variable_type_value_top_level_offers_primitives_and_constructors() {
    let u = uri("file:///v.tf");
    // Cursor right after `type = `.
    let src = "variable \"x\" {\n  type = \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 9)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    for t in ["string", "number", "bool", "any", "null"] {
        assert!(
            ls.contains(&t.to_string()),
            "primitive {t} missing; got {ls:?}"
        );
    }
    for c in ["list", "set", "map", "tuple", "object"] {
        assert!(
            ls.contains(&c.to_string()),
            "constructor {c} missing; got {ls:?}"
        );
    }
}

#[tokio::test]
async fn variable_type_value_inside_list_constructor_also_offers_types() {
    let u = uri("file:///v.tf");
    // Cursor inside `list(|)`.
    let src = "variable \"x\" {\n  type = list()\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 14)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"string".to_string()), "got {ls:?}");
    assert!(ls.contains(&"map".to_string()));
}

#[tokio::test]
async fn variable_type_value_inside_object_constructor() {
    let u = uri("file:///v.tf");
    // Cursor at `object({ name = | })`.
    let src = "variable \"x\" {\n  type = object({ name =  })\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 25)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"string".to_string()), "got {ls:?}");
    assert!(ls.contains(&"object".to_string()));
}

#[tokio::test]
async fn variable_type_value_does_not_fire_for_default() {
    // `default = ` is NOT a type expression — we should not suggest
    // `string`/`number` as values.
    let u = uri("file:///v.tf");
    let src = "variable \"x\" {\n  default = \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 12)),
    )
    .await
    .expect("ok");
    // We may or may not have other suggestions here; the assertion
    // is only that primitive type words aren't polluting the list.
    if let Some(CompletionResponse::Array(items)) = resp {
        let ls: Vec<_> = items.iter().map(|i| i.label.clone()).collect();
        assert!(
            !ls.contains(&"string".to_string()),
            "type primitives must not appear for default; got {ls:?}"
        );
    }
}

#[tokio::test]
async fn variable_block_body_suggests_standard_attrs() {
    let u = uri("file:///v.tf");
    let src = "variable \"my_var\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"type".to_string()));
    assert!(ls.contains(&"default".to_string()));
    assert!(ls.contains(&"description".to_string()));
    assert!(ls.contains(&"sensitive".to_string()));
    assert!(ls.contains(&"nullable".to_string()));
    assert!(ls.contains(&"validation".to_string()));
}

// --- Nested-block body routing (BuiltinNestedBody) ------------------------
//
// When the cursor is *inside* a nested block like `validation`,
// `precondition`, `lifecycle.postcondition`, or backend sub-blocks
// (`assume_role`, `endpoints`, `workspaces`, `exec`, `cloud.workspaces`),
// the completion dispatcher resolves the nested schema via the
// block-path classifier and offers that schema's attrs — *not* the
// enclosing block's attrs.

#[tokio::test]
async fn validation_body_suggests_condition_and_error_message() {
    let u = uri("file:///v.tf");
    let src = "variable \"x\" {\n  validation {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(
        ls.contains(&"condition".to_string()),
        "validation body must offer `condition`; got {ls:?}"
    );
    assert!(
        ls.contains(&"error_message".to_string()),
        "validation body must offer `error_message`; got {ls:?}"
    );
    // Must NOT leak the outer `variable` block's attrs.
    assert!(
        !ls.contains(&"type".to_string()) && !ls.contains(&"nullable".to_string()),
        "validation body must not show variable attrs; got {ls:?}"
    );
}

#[tokio::test]
async fn precondition_body_in_output_suggests_condition_and_error_message() {
    let u = uri("file:///o.tf");
    let src = "output \"x\" {\n  precondition {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"condition".to_string()), "got {ls:?}");
    assert!(ls.contains(&"error_message".to_string()), "got {ls:?}");
    assert!(!ls.contains(&"value".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn lifecycle_body_in_resource_suggests_lifecycle_attrs() {
    let u = uri("file:///r.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  lifecycle {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"create_before_destroy".to_string()), "got {ls:?}");
    assert!(ls.contains(&"prevent_destroy".to_string()), "got {ls:?}");
    assert!(ls.contains(&"ignore_changes".to_string()), "got {ls:?}");
    assert!(ls.contains(&"replace_triggered_by".to_string()), "got {ls:?}");
    assert!(ls.contains(&"precondition".to_string()), "got {ls:?}");
    assert!(ls.contains(&"postcondition".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn lifecycle_body_in_data_block_only_allows_postcondition() {
    let u = uri("file:///d.tf");
    let src = "data \"aws_ami\" \"x\" {\n  lifecycle {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"postcondition".to_string()), "got {ls:?}");
    // Resource-only attrs must not appear here.
    assert!(!ls.contains(&"create_before_destroy".to_string()), "got {ls:?}");
    assert!(!ls.contains(&"prevent_destroy".to_string()), "got {ls:?}");
    assert!(!ls.contains(&"precondition".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn postcondition_in_lifecycle_offers_condition_and_error_message() {
    let u = uri("file:///r.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  lifecycle {\n    postcondition {\n      \n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 6)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"condition".to_string()), "got {ls:?}");
    assert!(ls.contains(&"error_message".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn assume_role_body_in_s3_backend_suggests_role_arn() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  backend \"s3\" {\n    assume_role {\n      \n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 6)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"role_arn".to_string()), "got {ls:?}");
    assert!(ls.contains(&"session_name".to_string()), "got {ls:?}");
    // Must NOT leak S3 backend body attrs.
    assert!(!ls.contains(&"bucket".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn endpoints_body_in_s3_backend_suggests_service_overrides() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  backend \"s3\" {\n    endpoints {\n      \n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 6)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"s3".to_string()), "got {ls:?}");
    assert!(ls.contains(&"dynamodb".to_string()), "got {ls:?}");
    assert!(!ls.contains(&"bucket".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn workspaces_body_in_remote_backend_suggests_name_and_prefix() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  backend \"remote\" {\n    workspaces {\n      \n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 6)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"name".to_string()), "got {ls:?}");
    assert!(ls.contains(&"prefix".to_string()), "got {ls:?}");
    // Remote backend's own attrs must not leak through.
    assert!(!ls.contains(&"organization".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn cloud_body_suggests_org_and_workspaces() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  cloud {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"organization".to_string()), "got {ls:?}");
    assert!(ls.contains(&"hostname".to_string()), "got {ls:?}");
    assert!(ls.contains(&"workspaces".to_string()), "got {ls:?}");
    // Must not leak terraform-block attrs.
    assert!(!ls.contains(&"required_version".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn workspaces_body_in_cloud_suggests_name_prefix_tags() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  cloud {\n    workspaces {\n      \n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 6)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"name".to_string()), "got {ls:?}");
    assert!(ls.contains(&"tags".to_string()), "got {ls:?}");
    // Must not show cloud's own attrs.
    assert!(!ls.contains(&"organization".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn exec_body_in_kubernetes_backend_suggests_api_version_and_command() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  backend \"kubernetes\" {\n    exec {\n      \n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 6)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"api_version".to_string()), "got {ls:?}");
    assert!(ls.contains(&"command".to_string()), "got {ls:?}");
    assert!(ls.contains(&"args".to_string()), "got {ls:?}");
    assert!(ls.contains(&"env".to_string()), "got {ls:?}");
    // Must not leak kubernetes backend attrs.
    assert!(!ls.contains(&"namespace".to_string()), "got {ls:?}");
}

// Regression: the existing resource/data dispatch still fires for
// provider-schema-driven nested blocks (not lifecycle). The
// BuiltinNestedBody router must only intercept when the nested path
// passes through `lifecycle`.
#[tokio::test]
async fn resource_nested_block_uses_provider_schema_not_builtin_router() {
    // `root_block_device` is a provider-defined nested block inside
    // `aws_instance`; we shouldn't hijack it with the built-in
    // resolver. Expect NO completions because no provider schema is
    // installed, but importantly the context should not resolve to
    // BuiltinNestedBody (which would also yield nothing but for
    // different reasons). Verified behaviorally by absence of
    // crashes and absence of lifecycle attrs.
    let u = uri("file:///r.tf");
    let src = "resource \"aws_instance\" \"x\" {\n  root_block_device {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok");
    if let Some(CompletionResponse::Array(items)) = resp {
        let ls: Vec<_> = items.iter().map(|i| i.label.clone()).collect();
        // `create_before_destroy` only appears inside `lifecycle`; if
        // we hijacked the nested-block dispatch for arbitrary nested
        // blocks, it would show up here.
        assert!(
            !ls.contains(&"create_before_destroy".to_string()),
            "provider-schema path must not route through lifecycle; got {ls:?}"
        );
    }
}

// --- Nested-block snippet body pre-population -----------------------------
//
// When the user picks a nested block that has strictly-required
// inner attributes, the snippet should pre-fill those with numbered
// tabstops so they can tab through instead of landing in an empty
// block. Empty body (no required attrs) and labeled blocks with
// required attrs are also covered.

#[tokio::test]
async fn variable_validation_block_prefills_condition_and_error_message() {
    let u = uri("file:///v.tf");
    let src = "variable \"x\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "validation")
        .insert_text
        .expect("validation has insert_text");
    assert!(
        insert.contains("condition = ${1}"),
        "validation must prefill `condition`; got {insert:?}"
    );
    assert!(
        insert.contains("error_message = \"${2}\""),
        "validation must prefill quoted `error_message`; got {insert:?}"
    );
    // When the snippet already pre-fills required attributes, there
    // should NOT be an extra blank line before the closing brace —
    // that was the old behavior and it left a stray whitespace line
    // after tab-through.
    assert!(
        insert.ends_with("${2}\"\n}"),
        "validation must close directly after last required attr; got {insert:?}"
    );
    assert!(
        !insert.contains("\n  $0\n}"),
        "validation must not have a trailing $0 line causing a blank before the brace; got {insert:?}"
    );
}

#[tokio::test]
async fn output_precondition_block_prefills_required_pair() {
    let u = uri("file:///o.tf");
    let src = "output \"x\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "precondition")
        .insert_text
        .expect("precondition has insert_text");
    assert!(
        insert.contains("condition = ${1}") && insert.contains("error_message = \"${2}\""),
        "precondition must prefill condition + error_message; got {insert:?}"
    );
}

#[tokio::test]
async fn s3_backend_assume_role_block_prefills_role_arn() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  backend \"s3\" {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "assume_role")
        .insert_text
        .expect("assume_role has insert_text");
    assert!(
        insert.contains("role_arn = \"${1}\""),
        "assume_role must prefill role_arn; got {insert:?}"
    );
}

#[tokio::test]
async fn remote_backend_workspaces_block_prefills_name() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  backend \"remote\" {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "workspaces")
        .insert_text
        .expect("workspaces has insert_text");
    assert!(
        insert.contains("name = \"${1}\""),
        "workspaces must prefill name; got {insert:?}"
    );
}

#[tokio::test]
async fn kubernetes_backend_exec_block_prefills_api_version_and_command() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  backend \"kubernetes\" {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "exec")
        .insert_text
        .expect("exec has insert_text");
    assert!(
        insert.contains("api_version = \"${1}\"") && insert.contains("command = \"${2}\""),
        "exec must prefill api_version + command; got {insert:?}"
    );
}

// Regression: blocks without required_attrs must still render as
// empty-body snippets (single trailing $0), not emit stray tabstops.
#[tokio::test]
async fn terraform_block_required_providers_renders_as_empty_body() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "required_providers")
        .insert_text
        .expect("required_providers has insert_text");
    assert_eq!(insert, "required_providers {\n  $0\n}");
}

// Labeled blocks without required_attrs keep their label tabstop at
// $1 and their body $0 — no regression in the `backend "s3"` style
// snippet emitted at top level of `terraform { }`.
#[tokio::test]
async fn terraform_block_backend_keeps_label_tabstop() {
    let u = uri("file:///tf.tf");
    let src = "terraform {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "backend")
        .insert_text
        .expect("backend has insert_text");
    assert_eq!(insert, "backend \"${1:s3}\" {\n  $0\n}");
}

// After a completed `type = …` line the cursor should fall back to
// body-attribute completion, not keep firing the type-expression
// detector. Regression: previously the classifier latched onto the
// last depth-0 `=` forever, so even after `type = object({…})\n` the
// next line would show `string`, `number`, `list`, etc. instead of
// `description`, `validation`, `sensitive`, …
#[tokio::test]
async fn variable_body_after_complex_type_assignment_offers_attrs_not_type_primitives() {
    let u = uri("file:///v.tf");
    let src = "variable \"test\" {\n  type = object({\n    name = string\n  })\n  \n}\n";
    let backend = fresh_backend(src, &u);
    // Cursor on the blank line after the `})` that closes the type
    // expression.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(4, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(
        ls.contains(&"description".to_string()),
        "body position must offer body attrs; got {ls:?}"
    );
    assert!(
        ls.contains(&"validation".to_string()),
        "body position must offer `validation` block; got {ls:?}"
    );
    assert!(
        !ls.contains(&"string".to_string()),
        "body position must NOT offer type primitives; got {ls:?}"
    );
}

// The simpler form: `type = string\n` on one line, cursor on the next.
#[tokio::test]
async fn variable_body_after_primitive_type_assignment_offers_attrs() {
    let u = uri("file:///v.tf");
    let src = "variable \"x\" {\n  type = string\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"description".to_string()), "got {ls:?}");
    assert!(!ls.contains(&"number".to_string()), "got {ls:?}");
}

#[tokio::test]
async fn output_block_body_suggests_value_and_sensitive() {
    let u = uri("file:///o.tf");
    let src = "output \"my_output\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"value".to_string()));
    assert!(ls.contains(&"description".to_string()));
    assert!(ls.contains(&"sensitive".to_string()));
}

#[tokio::test]
async fn locals_block_body_returns_no_completions() {
    let u = uri("file:///l.tf");
    let src = "locals {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok");
    assert!(resp.is_none(), "locals body has no fixed schema; got {resp:?}");
}

#[tokio::test]
async fn backend_s3_body_suggests_bucket_key_region() {
    let u = uri("file:///b.tf");
    let src = "terraform {\n  backend \"s3\" {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"bucket".to_string()));
    assert!(ls.contains(&"key".to_string()));
    assert!(ls.contains(&"region".to_string()));
    // Should NOT leak terraform-block attrs.
    assert!(!ls.contains(&"required_version".to_string()));
}

#[tokio::test]
async fn backend_unknown_name_returns_no_completions() {
    let u = uri("file:///b.tf");
    let src = "terraform {\n  backend \"hypothetical\" {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok");
    assert!(
        resp.is_none(),
        "unknown backend must not leak any schema; got {resp:?}"
    );
}

#[tokio::test]
async fn required_providers_body_suggests_common_entries() {
    let u = uri("file:///rp.tf");
    let src = "terraform {\n  required_providers {\n    \n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"aws".to_string()));
    assert!(ls.contains(&"azurerm".to_string()));
    assert!(ls.contains(&"google".to_string()));
}

#[tokio::test]
async fn required_providers_entry_body_suggests_source_and_version() {
    let u = uri("file:///rpe.tf");
    let src = "terraform {\n  required_providers {\n    aws = {\n      \n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 6)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"source".to_string()));
    assert!(ls.contains(&"version".to_string()));
    assert!(ls.contains(&"configuration_aliases".to_string()));
}

#[tokio::test]
async fn required_provider_source_value_offers_curated_sources() {
    // Cursor inside `source = "|"` should surface the curated list of
    // common provider source paths (e.g. `hashicorp/aws`) as plain
    // string values.
    let u = uri("file:///src.tf");
    let src = "terraform {\n  required_providers {\n    aws = {\n      source = \"\"\n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    // Cursor between the two `"` on the source line (after `source = "`).
    // Line 3 = `      source = ""` — position 16 sits between the
    // two quotes (character 15 is the open `"`, 16 is the close `"`).
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 16)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    // Default (registry-less) form for each curated source.
    assert!(ls.contains(&"hashicorp/aws".to_string()), "got {ls:?}");
    assert!(ls.contains(&"hashicorp/azurerm".to_string()));
    // Explicit hostname-prefixed variants so users can pin a
    // specific registry instead of depending on CLI default.
    assert!(ls.contains(&"registry.terraform.io/hashicorp/aws".to_string()));
    assert!(ls.contains(&"registry.opentofu.org/hashicorp/aws".to_string()));
    assert!(ls.contains(&"integrations/github".to_string()));
    // Should NOT offer scaffold snippet-style labels (those belong to
    // the RequiredProvidersBody context, not the string-value one).
    assert!(!ls.contains(&"aws".to_string()));
}

#[tokio::test]
async fn required_provider_version_operator_items_have_descriptions() {
    // Sanity-check that operator completions carry both a `detail`
    // (one-liner) and a `documentation` body — the regression guard
    // for the constraint-aware completion wiring.
    let u = uri("file:///ops.tf");
    let src = "terraform {\n  required_providers {\n    aws = {\n      version = \"\"\n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    // Cursor at position 17 — between `"` `"` on the version line.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 17)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let CompletionResponse::Array(items) = resp else { panic!("expected array") };
    let tilde = items
        .iter()
        .find(|i| i.label == "~>")
        .expect("`~>` item must be present");
    assert!(tilde.detail.is_some(), "detail missing on `~>`");
    let doc = match &tilde.documentation {
        Some(lsp_types::Documentation::MarkupContent(mc)) => mc.value.clone(),
        _ => panic!("expected markup documentation on `~>`"),
    };
    assert!(
        doc.contains("patch updates"),
        "`~>` documentation must mention patch updates; got: {doc}"
    );
    // Also sanity check >= is present.
    assert!(items.iter().any(|i| i.label == ">="), "`>=` item missing");
}

#[tokio::test]
async fn module_version_value_offers_operators_at_start() {
    // Cursor inside `module "x" { version = "|" }` must now route to
    // the new ModuleVersionValue context and produce operator items.
    let u = uri("file:///m.tf");
    let src = "module \"x\" {\n  source  = \"terraform-aws-modules/vpc/aws\"\n  version = \"\"\n}\n";
    let backend = fresh_backend(src, &u);
    // Line 2 `  version = ""` — cursor between the two quotes.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 13)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&">=".to_string()), "got {ls:?}");
    assert!(ls.contains(&"~>".to_string()));
    assert!(ls.contains(&"=".to_string()));
}

#[tokio::test]
async fn required_version_value_offers_constraint_templates() {
    // Inside `terraform { required_version = "|" }`. We can't
    // guarantee GitHub is reachable (or that the CI has a populated
    // cache), so assert only the static constraint templates that
    // are always appended. If the network call succeeds, the real
    // versions show up alongside — the test just proves routing.
    let u = uri("file:///rv.tf");
    let src = "terraform {\n  required_version = \"\"\n}\n";
    let backend = fresh_backend(src, &u);
    // Line 1 = `  required_version = ""` — between the two quotes.
    // Indices 0-1 spaces, 2-17 `required_version`, 18 space, 19 `=`,
    // 20 space, 21 open `"`, 22 close `"`. Cursor at 22.
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 22)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&">=".to_string()), "static template missing; got {ls:?}");
    assert!(ls.contains(&"~>".to_string()));
    assert!(ls.contains(&"=".to_string()));
}

#[tokio::test]
async fn required_provider_version_value_offers_constraint_templates() {
    // Without a sibling `source`, we can't hit the registry — but the
    // static constraint operators should still show.
    let u = uri("file:///ver.tf");
    let src = "terraform {\n  required_providers {\n    aws = {\n      version = \"\"\n    }\n  }\n}\n";
    let backend = fresh_backend(src, &u);
    // Line 3 = `      version = ""` — position 17 is between the
    // two quotes (indices: 6-12 `version`, 13 space, 14 `=`, 15
    // space, 16 open `"`, 17 close `"`).
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 17)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"~>".to_string()), "pessimistic constraint missing; got {ls:?}");
    assert!(ls.contains(&">=".to_string()));
    assert!(ls.contains(&"=".to_string()));
}

#[tokio::test]
async fn provider_block_body_uses_provider_schema_plus_alias() {
    // `provider "aws" { }` should offer both the provider's own schema
    // attrs (here `region`) and the universal `alias` meta-arg.
    let u = uri("file:///p.tf");
    let src = "provider \"aws\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let schema: ProviderSchemas = sonic_rs::from_str(
        r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/hashicorp/aws": {
                "provider": {
                    "version": 0,
                    "block": {
                        "attributes": {
                            "region":  { "type": "string", "optional": true },
                            "profile": { "type": "string", "optional": true }
                        }
                    }
                },
                "resource_schemas": {},
                "data_source_schemas": {}
            }
        }
    }"#,
    )
    .expect("parse schema");
    backend.state.install_schemas(schema);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"region".to_string()));
    assert!(ls.contains(&"profile".to_string()));
    assert!(ls.contains(&"alias".to_string()));
}

// --- `description` / `default` nudge bundling -----------------------------
//
// The `variable` and `output` top-level scaffolds, and their body-level
// `type` / `value` items, bundle extra API-surface attributes
// (`default`, `description`) so the author is prompted to think about
// documentation and default-value semantics when defining a module's
// public interface. The bundling is gated on the attribute not being
// already present in the block.

fn find_item(resp: CompletionResponse, label: &str) -> CompletionItem {
    let items = match resp {
        CompletionResponse::Array(v) => v,
        CompletionResponse::List(l) => l.items,
    };
    items
        .into_iter()
        .find(|i| i.label == label)
        .unwrap_or_else(|| panic!("missing completion item {label}"))
}

#[tokio::test]
async fn top_level_variable_scaffold_bundles_default_and_description() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("", &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "variable")
        .insert_text
        .expect("variable has insert_text");
    assert!(
        insert.contains("default = "),
        "variable scaffold must bundle default = line; got {insert:?}"
    );
    assert!(
        insert.contains("description = \""),
        "variable scaffold must bundle description line; got {insert:?}"
    );
}

#[tokio::test]
async fn top_level_output_scaffold_bundles_description_but_not_default() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("", &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "output")
        .insert_text
        .expect("output has insert_text");
    assert!(
        insert.contains("description = \""),
        "output scaffold must bundle description line; got {insert:?}"
    );
    assert!(
        !insert.contains("default = "),
        "output scaffold must not include default (outputs have no default); got {insert:?}"
    );
}

// Attributes in the scaffolds are ordered alphabetically so the
// generated blocks read consistently and diff cleanly. The scaffold
// should also not trail a blank padding line before the closing
// brace.
#[tokio::test]
async fn top_level_variable_scaffold_orders_attrs_alphabetically_and_no_trailing_blank() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("", &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "variable")
        .insert_text
        .expect("variable has insert_text");
    let default_idx = insert.find("default = ").expect("has default");
    let desc_idx = insert.find("description = ").expect("has description");
    let type_idx = insert.find("type = ").expect("has type");
    assert!(
        default_idx < desc_idx && desc_idx < type_idx,
        "expected default → description → type (alphabetical) ordering; got {insert:?}"
    );
    assert!(
        !insert.contains("\n  \n}") && !insert.contains("\n\n}"),
        "scaffold must not have a blank/whitespace line before closing brace; got {insert:?}"
    );
}

#[tokio::test]
async fn top_level_output_scaffold_orders_attrs_alphabetically_and_no_trailing_blank() {
    let u = uri("file:///a.tf");
    let backend = fresh_backend("", &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(0, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "output")
        .insert_text
        .expect("output has insert_text");
    let desc_idx = insert.find("description = ").expect("has description");
    let value_idx = insert.find("value = ").expect("has value");
    assert!(
        desc_idx < value_idx,
        "expected description → value (alphabetical) ordering; got {insert:?}"
    );
    assert!(
        !insert.contains("\n  \n}") && !insert.contains("\n\n}"),
        "scaffold must not have a blank/whitespace line before closing brace; got {insert:?}"
    );
}

#[tokio::test]
async fn variable_body_type_bundles_default_and_description() {
    let u = uri("file:///v.tf");
    // Empty body — the `type` completion should append default + description.
    let src = "variable \"x\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "type")
        .insert_text
        .expect("type has insert_text");
    assert!(
        insert.starts_with("type = ${1:string}"),
        "type snippet should start with `type = ${{1:string}}`; got {insert:?}"
    );
    assert!(
        insert.contains("\ndefault = "),
        "type snippet should append default; got {insert:?}"
    );
    assert!(
        insert.contains("\ndescription = \""),
        "type snippet should append description; got {insert:?}"
    );
}

#[tokio::test]
async fn variable_body_type_skips_default_when_already_present() {
    let u = uri("file:///v.tf");
    let src = "variable \"x\" {\n  default = 1\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "type")
        .insert_text
        .expect("type has insert_text");
    assert!(
        !insert.contains("default = "),
        "type snippet must not duplicate default when present; got {insert:?}"
    );
    assert!(
        insert.contains("description = \""),
        "type snippet should still bundle description; got {insert:?}"
    );
}

#[tokio::test]
async fn variable_body_type_plain_when_all_companions_present() {
    let u = uri("file:///v.tf");
    let src = "variable \"x\" {\n  default = 1\n  description = \"y\"\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(3, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "type")
        .insert_text
        .expect("type has insert_text");
    assert_eq!(
        insert, "type = ${1:string}",
        "type snippet should be plain when default + description already present"
    );
}

#[tokio::test]
async fn output_body_value_bundles_description() {
    let u = uri("file:///o.tf");
    let src = "output \"x\" {\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(1, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "value")
        .insert_text
        .expect("value has insert_text");
    assert!(
        insert.starts_with("value = ${1}"),
        "value snippet should start with `value = ${{1}}`; got {insert:?}"
    );
    assert!(
        insert.contains("\ndescription = \""),
        "value snippet should append description; got {insert:?}"
    );
}

#[tokio::test]
async fn output_body_value_plain_when_description_present() {
    let u = uri("file:///o.tf");
    let src = "output \"x\" {\n  description = \"y\"\n  \n}\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&u, Position::new(2, 2)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let insert = find_item(resp, "value")
        .insert_text
        .expect("value has insert_text");
    assert_eq!(
        insert, "value = ${1}",
        "value snippet should be plain when description already present"
    );
}
