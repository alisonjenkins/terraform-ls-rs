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

// --- Code action: insert inferred `type = …` from default ----------
//
// Variables that have a `default` set but no `type` trigger
// `terraform_typed_variables`. The default value implies a type via
// `parse_value_shape` (already in tfls-core); the code action just
// renders the inferred shape and splices it as `type = …` into the
// block body.

async fn code_actions_for(
    backend: &Backend,
    u: &Url,
    diag_msg_filter: &str,
) -> Vec<CodeActionOrCommand> {
    let diags = compute_diagnostics(&backend.state, u);
    let diag = diags
        .iter()
        .find(|d| d.message.contains(diag_msg_filter))
        .cloned()
        .unwrap_or_else(|| panic!("no diagnostic matching {diag_msg_filter:?}; got {diags:?}"));
    tfls_lsp::handlers::code_action::code_action(
        backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: diag.range,
            context: CodeActionContext {
                diagnostics: vec![diag],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .unwrap_or_default()
}

fn first_inserted_text(actions: &[CodeActionOrCommand], u: &Url) -> String {
    let action = match actions.first().expect("at least one action") {
        CodeActionOrCommand::CodeAction(a) => a,
        other => panic!("expected CodeAction, got {other:?}"),
    };
    let edit = action.edit.as_ref().expect("edit");
    let changes = edit.changes.as_ref().expect("changes");
    let edits = changes.get(u).expect("edits for this uri");
    edits[0].new_text.clone()
}

#[tokio::test]
async fn code_action_inserts_inferred_string_type() {
    // The typed-variables diagnostic suppresses on unused-looking
    // root-module variables, so each test pairs the variable with a
    // reference to force the diag to fire.
    let u = uri("file:///vars.tf");
    let src = concat!(
        "variable \"region\" {\n",
        "  default = \"us-east-1\"\n",
        "}\n",
        "output \"r\" { value = var.region }\n",
    );
    let backend = fresh_backend(src, &u);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    let new_text = first_inserted_text(&actions, &u);
    assert!(
        new_text.contains("type = string"),
        "got: {new_text:?}"
    );
}

#[tokio::test]
async fn code_action_inserts_inferred_number_type() {
    let u = uri("file:///vars.tf");
    let src = concat!(
        "variable \"count\" {\n",
        "  default = 3\n",
        "}\n",
        "output \"c\" { value = var.count }\n",
    );
    let backend = fresh_backend(src, &u);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    let new_text = first_inserted_text(&actions, &u);
    assert!(
        new_text.contains("type = number"),
        "got: {new_text:?}"
    );
}

#[tokio::test]
async fn code_action_inserts_inferred_bool_type() {
    let u = uri("file:///vars.tf");
    let src = concat!(
        "variable \"enabled\" {\n",
        "  default = true\n",
        "}\n",
        "output \"e\" { value = var.enabled }\n",
    );
    let backend = fresh_backend(src, &u);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    let new_text = first_inserted_text(&actions, &u);
    assert!(
        new_text.contains("type = bool"),
        "got: {new_text:?}"
    );
}

#[tokio::test]
async fn code_action_inserts_inferred_object_type_with_nested_keys() {
    let u = uri("file:///vars.tf");
    let src = concat!(
        "variable \"server\" {\n",
        "  default = {\n",
        "    name = \"web\"\n",
        "    port = 8080\n",
        "    enabled = true\n",
        "  }\n",
        "}\n",
        "output \"s\" { value = var.server }\n",
    );
    let backend = fresh_backend(src, &u);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    let new_text = first_inserted_text(&actions, &u);
    // Object inference renders alphabetically (BTreeMap).
    assert!(
        new_text.contains("type = object({"),
        "got: {new_text:?}"
    );
    assert!(new_text.contains("name = string"), "got: {new_text:?}");
    assert!(new_text.contains("port = number"), "got: {new_text:?}");
    assert!(new_text.contains("enabled = bool"), "got: {new_text:?}");
}

#[tokio::test]
async fn code_action_offers_any_placeholder_when_default_resolves_to_any() {
    // Reference defaults can't be statically typed → no concrete
    // inference. The diag-driven action skips, but the
    // cursor-driven fallback emits a `type = any` placeholder so
    // the menu isn't empty when the user invokes on the block.
    let u = uri("file:///vars.tf");
    let src = concat!(
        "variable \"x\" {\n",
        "  default = var.y\n",
        "}\n",
        "variable \"y\" {\n",
        "  type = string\n",
        "}\n",
        "output \"x\" { value = var.x }\n",
    );
    let backend = fresh_backend(src, &u);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    let placeholders: Vec<_> = actions
        .iter()
        .filter_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca)
                if ca.title.contains("Set variable type to `any`") =>
            {
                Some(ca)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        placeholders.len(),
        1,
        "expected one `type = any` placeholder; got {actions:?}",
    );
}

#[tokio::test]
async fn code_action_offers_any_placeholder_for_empty_array_default() {
    // `default = []` is too ambiguous to suggest a concrete
    // collection type. The diag-driven path skips; cursor-driven
    // fallback offers `type = any` so the user has SOMETHING.
    let u = uri("file:///vars.tf");
    let src = concat!(
        "variable \"items\" {\n",
        "  default = []\n",
        "}\n",
        "output \"i\" { value = var.items }\n",
    );
    let backend = fresh_backend(src, &u);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    let placeholders: Vec<_> = actions
        .iter()
        .filter_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca)
                if ca.title.contains("Set variable type to `any`") =>
            {
                Some(ca)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        placeholders.len(),
        1,
        "expected one `type = any` placeholder; got {actions:?}",
    );
}

#[tokio::test]
async fn code_action_falls_back_to_assigned_types_when_no_default() {
    // Variable has NO `default` and NO `type`. The store carries an
    // assignment from a tfvars file (or a module caller — same map)
    // for this dir → use the merged inferred type.
    let u = uri("file:///mod/main.tf");
    let src = concat!(
        "variable \"region\" {}\n",
        "output \"r\" { value = var.region }\n",
    );
    let backend = fresh_backend(src, &u);

    use std::collections::HashMap;
    use std::path::PathBuf;
    use tfls_core::variable_type::{Primitive, VariableType};
    let mut for_dir: HashMap<String, Vec<VariableType>> = HashMap::new();
    for_dir.insert(
        "region".to_string(),
        vec![VariableType::Primitive(Primitive::String)],
    );
    backend
        .state
        .replace_assigned_variable_types(PathBuf::from("/mod"), for_dir);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    let new_text = first_inserted_text(&actions, &u);
    assert!(
        new_text.contains("type = string"),
        "got: {new_text:?}"
    );
    let action = match &actions[0] {
        CodeActionOrCommand::CodeAction(a) => a,
        _ => panic!(),
    };
    assert!(
        action.title.contains("tfvars / module callers"),
        "title should attribute the source: {:?}",
        action.title
    );
}

#[tokio::test]
async fn code_action_offers_any_placeholder_when_assigned_types_disagree() {
    // Two callers give DIFFERENT types (string vs number) → no
    // canonical inference. Diag-driven path skips; cursor-driven
    // fallback offers `type = any` placeholder.
    let u = uri("file:///mod/main.tf");
    let src = concat!(
        "variable \"thing\" {}\n",
        "output \"t\" { value = var.thing }\n",
    );
    let backend = fresh_backend(src, &u);

    use std::collections::HashMap;
    use std::path::PathBuf;
    use tfls_core::variable_type::{Primitive, VariableType};
    let mut for_dir: HashMap<String, Vec<VariableType>> = HashMap::new();
    for_dir.insert(
        "thing".to_string(),
        vec![
            VariableType::Primitive(Primitive::String),
            VariableType::Primitive(Primitive::Number),
        ],
    );
    backend
        .state
        .replace_assigned_variable_types(PathBuf::from("/mod"), for_dir);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    let placeholders: Vec<_> = actions
        .iter()
        .filter_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca)
                if ca.title.contains("Set variable type to `any`") =>
            {
                Some(ca)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        placeholders.len(),
        1,
        "expected one `type = any` placeholder; got {actions:?}",
    );
}

#[tokio::test]
async fn code_action_default_takes_priority_over_assigned_types() {
    // When both sources disagree, the variable's own `default` wins
    // (the author explicitly wrote it; assignments are external
    // observations).
    let u = uri("file:///mod/main.tf");
    let src = concat!(
        "variable \"region\" {\n",
        "  default = \"us-east-1\"\n",
        "}\n",
        "output \"r\" { value = var.region }\n",
    );
    let backend = fresh_backend(src, &u);

    use std::collections::HashMap;
    use std::path::PathBuf;
    use tfls_core::variable_type::{Primitive, VariableType};
    let mut for_dir: HashMap<String, Vec<VariableType>> = HashMap::new();
    for_dir.insert(
        "region".to_string(),
        vec![VariableType::Primitive(Primitive::Number)],
    );
    backend
        .state
        .replace_assigned_variable_types(PathBuf::from("/mod"), for_dir);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    let new_text = first_inserted_text(&actions, &u);
    assert!(
        new_text.contains("type = string"),
        "default should win over assigned-types disagreement; got: {new_text:?}"
    );
}

#[tokio::test]
async fn code_action_skips_when_block_already_has_type() {
    // Defensive: if a stale diagnostic for an already-typed
    // variable somehow reaches the action handler, no edit fires.
    let u = uri("file:///vars.tf");
    let src = "variable \"region\" {\n  type    = string\n  default = \"us-east-1\"\n}\n";
    let backend = fresh_backend(src, &u);

    // Synthesise a `variable has no type` diagnostic for `region` —
    // the diagnostic rule won't actually emit one here, but a stale
    // pull cache could. The handler must refuse.
    use tower_lsp::lsp_types::Diagnostic;
    let diag = Diagnostic {
        range: Range {
            start: Position::new(0, 0),
            end: Position::new(0, 8),
        },
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("terraform-ls-rs".to_string()),
        message: "`region` variable has no type".to_string(),
        ..Default::default()
    };

    let actions = tfls_lsp::handlers::code_action::code_action(
        &backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: diag.range,
            context: CodeActionContext {
                diagnostics: vec![diag],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .unwrap_or_default();
    // Move-variables / move-outputs may still fire (file is
    // `vars.tf`, not `variables.tf` / `outputs.tf`). What MUST
    // not fire is anything touching the block's `type`.
    let any_set_type = actions.iter().any(|a| match a {
        CodeActionOrCommand::CodeAction(ca) => ca.title.starts_with("Set variable type"),
        _ => false,
    });
    assert!(
        !any_set_type,
        "must not offer set-type action when block already has type; got {actions:?}"
    );
}

/// End-to-end pin for the env-split tfvars layout the user was hitting:
/// root module declares `variable "envtype" {}` (no type, no default),
/// and `params/{nonprod,prod}/params.tfvars` each assign
/// `envtype = "..."`. Confirms that `rebuild_assigned_variable_types_for_dir`
/// stages those assignments under the root dir and the code-action
/// handler returns the inferred quick-fix.
#[tokio::test]
async fn end_to_end_envtype_inference_via_subdir_tfvars() {
    use std::fs;

    // Real tmpdir — `discover_tfvars_attributable_to` walks the FS.
    let workspace = std::env::temp_dir().join(format!(
        "tfls-e2e-envtype-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(workspace.join("params/nonprod")).unwrap();
    fs::create_dir_all(workspace.join("params/prod")).unwrap();

    let main_tf = workspace.join("variables.tf");
    fs::write(
        &main_tf,
        "variable \"envtype\" {}\n\
         output \"e\" { value = var.envtype }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("params/nonprod/params.tfvars"),
        "envtype = \"nonprod\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("params/prod/params.tfvars"),
        "envtype = \"prod\"\n",
    )
    .unwrap();

    let u = Url::from_file_path(&main_tf).expect("file uri");
    let backend = fresh_backend(
        "variable \"envtype\" {}\noutput \"e\" { value = var.envtype }\n",
        &u,
    );

    // Run the indexer's per-dir rebuild — same call site bulk scan
    // and `did_open` use in production.
    tfls_lsp::indexer::rebuild_assigned_variable_types_for_dir(&backend.state, &workspace);

    // Sanity-check the staged map directly.
    let merged = backend.state.merged_assigned_type(&workspace, "envtype");
    assert_eq!(
        merged,
        Some(tfls_core::variable_type::VariableType::Primitive(
            tfls_core::variable_type::Primitive::String
        )),
        "section 1 should stage envtype as String from both tfvars; got {merged:?}",
    );

    // Now confirm the full code-action path returns a quick-fix.
    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    assert!(!actions.is_empty(), "expected quick-fix; got none");
    let new_text = first_inserted_text(&actions, &u);
    assert!(
        new_text.contains("type = string"),
        "quick-fix should insert `type = string`; got: {new_text:?}"
    );

    fs::remove_dir_all(&workspace).ok();
}

#[tokio::test]
async fn code_action_fix_all_inserts_types_for_every_untyped_variable() {
    // Three untyped variables in one file. Two have defaults that
    // imply a concrete type; one has nothing. The fix-all action
    // should produce a single workspace edit that types both
    // resolvable variables and skips the unresolvable one.
    let u = uri("file:///mod/main.tf");
    let src = concat!(
        "variable \"region\" {\n",
        "  default = \"us-east-1\"\n",
        "}\n",
        "variable \"port\" {\n",
        "  default = 8080\n",
        "}\n",
        "variable \"unknown\" {}\n",
        "output \"r\" { value = var.region }\n",
        "output \"p\" { value = var.port }\n",
        "output \"u\" { value = var.unknown }\n",
    );
    let backend = fresh_backend(src, &u);

    // Trigger code_action with NO context diagnostics — the fix-all
    // action is a source-level action that surfaces independently.
    let resp = tfls_lsp::handlers::code_action::code_action(
        &backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
            context: CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("some response");

    let fix_all = resp
        .iter()
        .find_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca)
                if ca.title.starts_with("Set variable types: infer") =>
            {
                Some(ca)
            }
            _ => None,
        })
        .expect("fix-all action present");

    let edits = fix_all
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .expect("edits for this uri");
    assert_eq!(edits.len(), 2, "should fix exactly 2 of 3 variables");
    let combined: String = edits.iter().map(|e| e.new_text.as_str()).collect();
    assert!(combined.contains("type = string"));
    assert!(combined.contains("type = number"));
}

#[tokio::test]
async fn code_action_inserts_inferred_type_from_cursor_without_diagnostic() {
    // The typed-variables warning's range covers only the
    // `variable` keyword, so nvim won't include the diag in
    // `params.context.diagnostics` when the cursor is on the
    // block label or interior. The cursor-position fallback path
    // should still produce the per-variable quick-fix.
    let u = uri("file:///mod/main.tf");
    let src = "variable \"region\" {\n  default = \"us-east-1\"\n}\n";
    let backend = fresh_backend(src, &u);

    // Cursor on the block label (col 13 of line 0), NOT on the
    // `variable` keyword.
    let resp = tfls_lsp::handlers::code_action::code_action(
        &backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: Range {
                start: Position::new(0, 13),
                end: Position::new(0, 13),
            },
            context: CodeActionContext {
                // Empty — simulate nvim sending no overlapping diags.
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("response");

    let action = resp
        .iter()
        .find_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca)
                if ca.title.contains("Set variable type to `string`") =>
            {
                Some(ca)
            }
            _ => None,
        })
        .expect("per-variable quick-fix should appear from cursor in block");
    let edits = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .expect("edits");
    assert_eq!(edits.len(), 1);
    assert!(edits[0].new_text.contains("type = string"));
}

#[tokio::test]
async fn code_action_offers_any_placeholder_when_no_inference() {
    // Variable declared but no default, no caller in workspace
    // → no inference. The cursor-driven path should still offer
    // a `type = any` placeholder so the user has SOMETHING to
    // pick from the menu rather than only the file-wide fix-all.
    let u = uri("file:///mod/main.tf");
    let src = "variable \"unknown\" {}\noutput \"u\" { value = var.unknown }\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::code_action::code_action(
        &backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: Range {
                start: Position::new(0, 13),
                end: Position::new(0, 13),
            },
            context: CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("response");
    let action = resp
        .iter()
        .find_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca)
                if ca.title.contains("Set variable type to `any`") =>
            {
                Some(ca)
            }
            _ => None,
        })
        .expect("placeholder action when no inference");
    let edits = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .expect("edits");
    assert!(edits[0].new_text.contains("type = any"));
}

#[tokio::test]
async fn code_action_unwraps_deprecated_interpolation() {
    let u = uri("file:///mod/main.tf");
    let src = "variable \"region\" { default = \"x\" }\n\
               output \"r\" { value = \"${var.region}\" }\n";
    let backend = fresh_backend(src, &u);
    let actions = code_actions_for(&backend, &u, "interpolation-only").await;
    let new_text = first_inserted_text(&actions, &u);
    assert_eq!(new_text, "var.region", "got: {new_text:?}");
}

#[tokio::test]
async fn code_action_unwraps_interpolation_with_inner_braces() {
    // Object literal inside `${…}` shouldn't confuse the brace
    // balancer.
    let u = uri("file:///mod/main.tf");
    let src = "output \"r\" { value = \"${tomap({a=1})}\" }\n";
    let backend = fresh_backend(src, &u);
    let actions = code_actions_for(&backend, &u, "interpolation-only").await;
    let new_text = first_inserted_text(&actions, &u);
    assert_eq!(new_text, "tomap({a=1})", "got: {new_text:?}");
}

#[tokio::test]
async fn code_action_declares_undefined_variables() {
    // Three `var.X` references; only one is declared. Active file
    // is `main.tf` — declarations must NOT land in main.tf
    // (would trip terraform_standard_module_structure). They go
    // into `<module-dir>/variables.tf`. variables.tf doesn't
    // exist on disk, so the WorkspaceEdit needs a CreateFile op.
    let u = uri("file:///nonexistent-mod-for-create/main.tf");
    let src = "variable \"declared\" { default = \"x\" }\n\
               output \"a\" { value = var.declared }\n\
               output \"b\" { value = var.missing_a }\n\
               output \"c\" { value = var.missing_b }\n\
               output \"d\" { value = var.missing_a }\n";
    let backend = fresh_backend(src, &u);

    let resp = tfls_lsp::handlers::code_action::code_action(
        &backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
            context: CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("response");
    let action = resp
        .iter()
        .find_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca)
                if ca.title.starts_with("Declare 2 undefined variables") =>
            {
                Some(ca)
            }
            _ => None,
        })
        .expect("declare-undefined action present");
    // Active file is main.tf — must NOT be edited.
    if let Some(changes) = action.edit.as_ref().and_then(|e| e.changes.as_ref()) {
        assert!(!changes.contains_key(&u), "main.tf must not be edited");
    }
    // Target file does not exist → expect documentChanges with
    // a CreateFile op for variables.tf + an initial insert.
    use tower_lsp::lsp_types::{
        DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp,
    };
    let dc = action
        .edit
        .as_ref()
        .and_then(|e| e.document_changes.as_ref())
        .expect("documentChanges present");
    let ops = match dc {
        DocumentChanges::Operations(o) => o,
        _ => panic!("expected Operations variant"),
    };
    let target_url = uri("file:///nonexistent-mod-for-create/variables.tf");
    let mut saw_create = false;
    let mut saw_edit_with_blocks = false;
    for op in ops {
        match op {
            DocumentChangeOperation::Op(ResourceOp::Create(c)) => {
                assert_eq!(c.uri, target_url);
                saw_create = true;
            }
            DocumentChangeOperation::Edit(te) => {
                assert_eq!(te.text_document.uri, target_url);
                for e in &te.edits {
                    let OneOf::Left(edit) = e else {
                        panic!("annotated edit not expected");
                    };
                    if edit.new_text.contains("variable \"missing_a\" {}")
                        && edit.new_text.contains("variable \"missing_b\" {}")
                        && !edit.new_text.contains("variable \"declared\"")
                    {
                        saw_edit_with_blocks = true;
                    }
                }
            }
            _ => {}
        }
    }
    assert!(saw_create, "CreateFile op for variables.tf");
    assert!(saw_edit_with_blocks, "TextEdit inserts the missing stubs");
}

#[tokio::test]
async fn code_action_converts_2arg_lookup_to_index() {
    let u = uri("file:///mod/main.tf");
    let src = "variable \"m\" { default = { k = \"v\" } }\n\
               output \"o\" { value = lookup(var.m, \"k\") }\n";
    let backend = fresh_backend(src, &u);
    let actions = code_actions_for(&backend, &u, "two-argument `lookup()`").await;
    let new_text = first_inserted_text(&actions, &u);
    assert_eq!(new_text, "var.m[\"k\"]", "got: {new_text:?}");
}

#[tokio::test]
async fn code_action_refines_type_any_to_inferred() {
    // Two `type = any` variables. One has a concrete default
    // (string), one has a tfvars assignment (number via assigned
    // map). One has `type = list(any)` and shouldn't be touched
    // (parametrised, not bare `any`). One has `type = any` but no
    // signal — should also be skipped.
    let u = uri("file:///mod/main.tf");
    let src = concat!(
        "variable \"region\" {\n  type = any\n  default = \"us-east-1\"\n}\n",
        "variable \"port\" {\n  type = any\n}\n",
        "variable \"tags\" {\n  type = list(any)\n  default = []\n}\n",
        "variable \"unknown\" {\n  type = any\n}\n",
        "output \"r\" { value = var.region }\n",
        "output \"p\" { value = var.port }\n",
        "output \"u\" { value = var.unknown }\n",
    );
    let backend = fresh_backend(src, &u);

    use std::collections::HashMap;
    use std::path::PathBuf;
    use tfls_core::variable_type::{Primitive, VariableType};
    let mut for_dir: HashMap<String, Vec<VariableType>> = HashMap::new();
    for_dir.insert(
        "port".to_string(),
        vec![VariableType::Primitive(Primitive::Number)],
    );
    backend
        .state
        .replace_assigned_variable_types(PathBuf::from("/mod"), for_dir);

    let resp = tfls_lsp::handlers::code_action::code_action(
        &backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
            context: CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .expect("response");

    let action = resp
        .iter()
        .find_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca)
                if ca.title.starts_with("Refine 2 `type = any` variables") =>
            {
                Some(ca)
            }
            _ => None,
        })
        .expect("refine action present");
    let edits = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .expect("edits");
    assert_eq!(edits.len(), 2, "should refine exactly 2 of 4 vars");
    let texts: Vec<&str> = edits.iter().map(|e| e.new_text.as_str()).collect();
    assert!(texts.contains(&"string"), "got: {texts:?}");
    assert!(texts.contains(&"number"), "got: {texts:?}");
}

#[tokio::test]
async fn code_action_refine_any_skips_when_nothing_qualifies() {
    let u = uri("file:///mod/main.tf");
    let src = "variable \"x\" { type = string }\nvariable \"y\" { type = list(any) default = [] }\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::code_action::code_action(
        &backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
            context: CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok");
    let actions = resp.unwrap_or_default();
    let any_refine = actions.iter().any(|a| match a {
        CodeActionOrCommand::CodeAction(ca) => ca.title.starts_with("Refine ") && ca.title.contains("`type = any`"),
        _ => false,
    });
    assert!(!any_refine, "no refine action when nothing qualifies");
}

#[tokio::test]
async fn code_action_lookup_to_index_handles_complex_first_arg() {
    // First arg is a function call — index notation must wrap the
    // ENTIRE first-arg expression, not just its tail.
    let u = uri("file:///mod/main.tf");
    let src = "variable \"a\" { default = {} }\n\
               variable \"b\" { default = {} }\n\
               output \"o\" { value = lookup(merge(var.a, var.b), \"key\") }\n";
    let backend = fresh_backend(src, &u);
    let actions = code_actions_for(&backend, &u, "two-argument `lookup()`").await;
    let new_text = first_inserted_text(&actions, &u);
    assert_eq!(new_text, "merge(var.a, var.b)[\"key\"]", "got: {new_text:?}");
}

#[tokio::test]
async fn code_action_declare_undefined_skips_when_all_resolved() {
    let u = uri("file:///mod/main.tf");
    let src = "variable \"a\" { default = \"x\" }\n\
               output \"o\" { value = var.a }\n";
    let backend = fresh_backend(src, &u);
    let resp = tfls_lsp::handlers::code_action::code_action(
        &backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
            context: CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok");
    let actions = resp.unwrap_or_default();
    let any_declare = actions.iter().any(|a| match a {
        CodeActionOrCommand::CodeAction(ca) => ca.title.starts_with("Declare"),
        _ => false,
    });
    assert!(!any_declare, "no declare action when all vars resolved");
}

// -- Multi-scope code action tests -------------------------------------

/// Insert a sibling `.tf` doc into the backend so module-scope
/// iteration sees more than just the active file.
fn add_doc(backend: &Backend, u: &Url, src: &str) {
    backend
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));
}

/// Drive `code_action()` with no diagnostics + an empty range —
/// matches the cursor-only invocation an editor sends when the
/// user opens the menu without a visual selection.
async fn all_actions_for(backend: &Backend, u: &Url) -> Vec<CodeActionOrCommand> {
    tfls_lsp::handlers::code_action::code_action(
        backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
            context: CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .unwrap_or_default()
}

/// Same as `all_actions_for` but driven with a non-empty visual
/// selection so the Selection scope kicks in.
async fn all_actions_for_selection(
    backend: &Backend,
    u: &Url,
    range: Range,
) -> Vec<CodeActionOrCommand> {
    tfls_lsp::handlers::code_action::code_action(
        backend,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            range,
            context: CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
    .expect("ok")
    .unwrap_or_default()
}

fn find_action<'a>(
    actions: &'a [CodeActionOrCommand],
    title_prefix: &str,
) -> &'a tower_lsp::lsp_types::CodeAction {
    actions
        .iter()
        .find_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca) if ca.title.starts_with(title_prefix) => Some(ca),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no action with prefix {title_prefix:?}"))
}

#[tokio::test]
async fn scope_unwrap_interpolation_module_covers_all_files() {
    // Three .tf files in /mod, each with a single deprecated
    // `"${expr}"`. The Module-scope action's WorkspaceEdit must
    // cover all three URIs.
    let a = uri("file:///mod/a.tf");
    let b = uri("file:///mod/b.tf");
    let c = uri("file:///mod/c.tf");
    let backend = fresh_backend(
        "output \"a\" { value = \"${var.a}\" }\nvariable \"a\" {}\n",
        &a,
    );
    add_doc(
        &backend,
        &b,
        "output \"b\" { value = \"${var.b}\" }\nvariable \"b\" {}\n",
    );
    add_doc(
        &backend,
        &c,
        "output \"c\" { value = \"${var.c}\" }\nvariable \"c\" {}\n",
    );

    let actions = all_actions_for(&backend, &a).await;
    let action = find_action(&actions, "Unwrap 3 deprecated interpolations in this module");
    let changes = action.edit.as_ref().and_then(|e| e.changes.as_ref()).unwrap();
    assert_eq!(changes.len(), 3, "all three module files");
    for url in [&a, &b, &c] {
        assert!(changes.contains_key(url), "{url} edited");
    }
}

#[tokio::test]
async fn scope_unwrap_interpolation_selection_filters_by_range() {
    // Four deprecated interpolations on lines 1, 5, 10, 15.
    // Selection covering lines 4..=11 should keep edits on 5 + 10
    // only.
    let u = uri("file:///mod/main.tf");
    let mut src = String::new();
    src.push_str("output \"o0\" { value = \"${var.x}\" }\n"); // line 0
    for i in 1..=15 {
        if [1, 5, 10, 15].contains(&i) {
            src.push_str(&format!(
                "output \"o{i}\" {{ value = \"${{var.v{i}}}\" }}\n"
            ));
        } else {
            src.push_str(&format!("variable \"v{i}\" {{}}\n"));
        }
    }
    let backend = fresh_backend(&src, &u);

    let selection = Range {
        start: Position::new(4, 0),
        end: Position::new(11, 0),
    };
    let actions = all_actions_for_selection(&backend, &u, selection).await;
    let action = find_action(&actions, "Unwrap 2 deprecated interpolations in selection");
    let edits = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .unwrap();
    assert_eq!(edits.len(), 2, "selection narrows to lines 5 + 10");
}

#[tokio::test]
async fn scope_lookup_to_index_workspace_covers_unrelated_dirs() {
    // Two files in different module dirs each with a 2-arg lookup.
    // Workspace scope MUST cover both; Module scope only one.
    let a = uri("file:///modA/main.tf");
    let b = uri("file:///modB/main.tf");
    let backend = fresh_backend(
        "variable \"m\" { default = { k = \"v\" } }\noutput \"o\" { value = lookup(var.m, \"k\") }\n",
        &a,
    );
    add_doc(
        &backend,
        &b,
        "variable \"m\" { default = { k = \"v\" } }\noutput \"o\" { value = lookup(var.m, \"k\") }\n",
    );

    let actions = all_actions_for(&backend, &a).await;

    let module = find_action(&actions, "Convert 1 deprecated lookup in this module");
    let mod_changes = module
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .unwrap();
    assert_eq!(mod_changes.len(), 1, "module scope = one dir");
    assert!(mod_changes.contains_key(&a));

    let workspace = find_action(&actions, "Convert 2 deprecated lookups in workspace");
    let ws_changes = workspace
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .unwrap();
    assert_eq!(ws_changes.len(), 2, "workspace scope = both dirs");
    assert!(ws_changes.contains_key(&a));
    assert!(ws_changes.contains_key(&b));
}

#[tokio::test]
async fn scope_set_variable_types_module_aggregates_inferences() {
    // Two .tf files in the same module, each with one untyped
    // variable that has a usable `default`. Module scope should
    // emit edits for BOTH variables across both files.
    let a = uri("file:///mod/a.tf");
    let b = uri("file:///mod/b.tf");
    let backend = fresh_backend("variable \"region\" { default = \"us-east-1\" }\n", &a);
    add_doc(&backend, &b, "variable \"port\" { default = 8080 }\n");

    let actions = all_actions_for(&backend, &a).await;
    let module = find_action(&actions, "Set 2 untyped variables in this module");
    let changes = module
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes.get(&a).map(Vec::len), Some(1));
    assert_eq!(changes.get(&b).map(Vec::len), Some(1));
}

#[tokio::test]
async fn scope_declare_undefined_module_uses_union_of_declarations() {
    // /nonexistent-mod-A/a.tf declares `var.shared`; /…/b.tf
    // references it. Module scope must drop `shared`. Active
    // file is b.tf — declarations land in the module's
    // variables.tf (created since it doesn't exist).
    let a = uri("file:///nonexistent-mod-A/a.tf");
    let b = uri("file:///nonexistent-mod-A/b.tf");
    let backend = fresh_backend("variable \"shared\" {}\n", &a);
    add_doc(
        &backend,
        &b,
        "output \"o\" { value = var.shared }\noutput \"p\" { value = var.only_in_b }\n",
    );

    let actions = all_actions_for(&backend, &b).await;
    let module = find_action(&actions, "Declare 1 undefined variable in this module");

    // Active file (b.tf) and sibling (a.tf) MUST NOT be edited.
    if let Some(changes) = module.edit.as_ref().and_then(|e| e.changes.as_ref()) {
        assert!(!changes.contains_key(&b), "b.tf must not be edited");
        assert!(!changes.contains_key(&a), "a.tf must not be edited");
    }
    use tower_lsp::lsp_types::{
        DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp,
    };
    let dc = module
        .edit
        .as_ref()
        .and_then(|e| e.document_changes.as_ref())
        .expect("documentChanges present");
    let ops = match dc {
        DocumentChanges::Operations(o) => o,
        _ => panic!("expected Operations"),
    };
    let target = uri("file:///nonexistent-mod-A/variables.tf");
    let mut saw_create = false;
    let mut new_text_combined = String::new();
    for op in ops {
        match op {
            DocumentChangeOperation::Op(ResourceOp::Create(c)) => {
                assert_eq!(c.uri, target);
                saw_create = true;
            }
            DocumentChangeOperation::Edit(te) => {
                assert_eq!(te.text_document.uri, target);
                for e in &te.edits {
                    let OneOf::Left(edit) = e else {
                        panic!("annotated edit not expected");
                    };
                    new_text_combined.push_str(&edit.new_text);
                }
            }
            _ => {}
        }
    }
    assert!(saw_create);
    assert!(new_text_combined.contains("variable \"only_in_b\" {}"));
    assert!(!new_text_combined.contains("variable \"shared\""));
}

#[tokio::test]
async fn move_outputs_creates_outputs_tf_when_missing() {
    // main.tf has 2 outputs; no outputs.tf exists. Action emits
    // documentChanges:
    //   - delete both outputs from main.tf
    //   - create outputs.tf with both block sources concatenated
    let main = uri("file:///nonexistent-mod-mo/main.tf");
    let src = "resource \"null_resource\" \"r\" {}\n\
               output \"a\" { value = 1 }\n\
               output \"b\" { value = 2 }\n";
    let backend = fresh_backend(src, &main);
    let actions = all_actions_for(&backend, &main).await;
    let action = find_action(&actions, "Move 2 output blocks in this module");

    use tower_lsp::lsp_types::{
        DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp,
    };
    let target = uri("file:///nonexistent-mod-mo/outputs.tf");
    let dc = action
        .edit
        .as_ref()
        .and_then(|e| e.document_changes.as_ref())
        .expect("documentChanges present");
    let ops = match dc {
        DocumentChanges::Operations(o) => o,
        _ => panic!("expected Operations"),
    };
    let mut saw_create = false;
    let mut saw_main_delete_count = 0usize;
    let mut saw_target_text = String::new();
    for op in ops {
        match op {
            DocumentChangeOperation::Op(ResourceOp::Create(c)) => {
                assert_eq!(c.uri, target);
                saw_create = true;
            }
            DocumentChangeOperation::Edit(te) => {
                if te.text_document.uri == main {
                    for e in &te.edits {
                        let OneOf::Left(edit) = e else {
                            panic!("annotated edit not expected");
                        };
                        assert!(
                            edit.new_text.is_empty(),
                            "main edits delete only, got {:?}",
                            edit.new_text
                        );
                        saw_main_delete_count += 1;
                    }
                } else if te.text_document.uri == target {
                    for e in &te.edits {
                        let OneOf::Left(edit) = e else {
                            panic!("annotated edit not expected");
                        };
                        saw_target_text.push_str(&edit.new_text);
                    }
                }
            }
            _ => {}
        }
    }
    assert!(saw_create, "CreateFile op for outputs.tf");
    assert_eq!(saw_main_delete_count, 2, "two deletions on main.tf");
    assert!(saw_target_text.contains("output \"a\""), "got {saw_target_text:?}");
    assert!(saw_target_text.contains("output \"b\""), "got {saw_target_text:?}");
}

#[tokio::test]
async fn move_outputs_skips_outputs_tf_itself() {
    // outputs.tf already exists in state. main.tf has 1 output.
    // Move action should: delete from main, append to outputs.tf
    // (NOT create), and NOT also pull outputs out of outputs.tf.
    let main = uri("file:///nonexistent-mod-mo2/main.tf");
    let outputs = uri("file:///nonexistent-mod-mo2/outputs.tf");
    let backend = fresh_backend(
        "resource \"null_resource\" \"r\" {}\noutput \"a\" { value = 1 }\n",
        &main,
    );
    add_doc(&backend, &outputs, "output \"existing\" { value = 0 }\n");
    let actions = all_actions_for(&backend, &main).await;
    let action = find_action(&actions, "Move 1 output block in this module");

    use tower_lsp::lsp_types::{
        DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp,
    };
    let dc = action
        .edit
        .as_ref()
        .and_then(|e| e.document_changes.as_ref())
        .expect("documentChanges present");
    let ops = match dc {
        DocumentChanges::Operations(o) => o,
        _ => panic!("expected Operations"),
    };
    for op in ops {
        if matches!(op, DocumentChangeOperation::Op(ResourceOp::Create(_))) {
            panic!("must NOT create existing outputs.tf");
        }
        if let DocumentChangeOperation::Edit(te) = op {
            if te.text_document.uri == outputs {
                for e in &te.edits {
                    let OneOf::Left(edit) = e else { continue };
                    assert!(
                        edit.new_text.contains("output \"a\""),
                        "outputs.tf gains the moved output"
                    );
                }
            }
            if te.text_document.uri == main {
                for e in &te.edits {
                    let OneOf::Left(edit) = e else { continue };
                    assert!(
                        edit.new_text.is_empty(),
                        "main delete: empty new_text"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn move_variables_creates_variables_tf_when_missing() {
    // main.tf has 2 variable blocks; no variables.tf exists. Action
    // should delete from main + create variables.tf with both.
    let main = uri("file:///nonexistent-mod-mv/main.tf");
    let src = "resource \"null_resource\" \"r\" {}\n\
               variable \"a\" {}\n\
               variable \"b\" { default = 1 }\n";
    let backend = fresh_backend(src, &main);
    let actions = all_actions_for(&backend, &main).await;
    let action = find_action(&actions, "Move 2 variable blocks in this module");

    use tower_lsp::lsp_types::{
        DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp,
    };
    let target = uri("file:///nonexistent-mod-mv/variables.tf");
    let dc = action
        .edit
        .as_ref()
        .and_then(|e| e.document_changes.as_ref())
        .expect("documentChanges present");
    let ops = match dc {
        DocumentChanges::Operations(o) => o,
        _ => panic!("expected Operations"),
    };
    let mut saw_create = false;
    let mut delete_count = 0usize;
    let mut target_text = String::new();
    for op in ops {
        match op {
            DocumentChangeOperation::Op(ResourceOp::Create(c)) => {
                assert_eq!(c.uri, target);
                saw_create = true;
            }
            DocumentChangeOperation::Edit(te) => {
                if te.text_document.uri == main {
                    for e in &te.edits {
                        let OneOf::Left(edit) = e else {
                            panic!("annotated edit not expected");
                        };
                        assert!(edit.new_text.is_empty());
                        delete_count += 1;
                    }
                } else if te.text_document.uri == target {
                    for e in &te.edits {
                        let OneOf::Left(edit) = e else { continue };
                        target_text.push_str(&edit.new_text);
                    }
                }
            }
            _ => {}
        }
    }
    assert!(saw_create);
    assert_eq!(delete_count, 2);
    assert!(target_text.contains("variable \"a\""));
    assert!(target_text.contains("variable \"b\""));
}

#[tokio::test]
async fn move_variables_skips_variables_tf_itself() {
    // variables.tf already exists; main.tf has 1 variable. Action
    // appends to existing variables.tf, doesn't create.
    let main = uri("file:///nonexistent-mod-mv2/main.tf");
    let vars = uri("file:///nonexistent-mod-mv2/variables.tf");
    let backend = fresh_backend(
        "resource \"null_resource\" \"r\" {}\nvariable \"a\" {}\n",
        &main,
    );
    add_doc(&backend, &vars, "variable \"existing\" {}\n");
    let actions = all_actions_for(&backend, &main).await;
    let action = find_action(&actions, "Move 1 variable block in this module");

    use tower_lsp::lsp_types::{
        DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp,
    };
    let dc = action
        .edit
        .as_ref()
        .and_then(|e| e.document_changes.as_ref())
        .expect("documentChanges present");
    let ops = match dc {
        DocumentChanges::Operations(o) => o,
        _ => panic!("expected Operations"),
    };
    for op in ops {
        if matches!(op, DocumentChangeOperation::Op(ResourceOp::Create(_))) {
            panic!("must NOT create existing variables.tf");
        }
        if let DocumentChangeOperation::Edit(te) = op {
            if te.text_document.uri == vars {
                for e in &te.edits {
                    let OneOf::Left(edit) = e else { continue };
                    assert!(edit.new_text.contains("variable \"a\""));
                }
            }
        }
    }
}

#[tokio::test]
async fn move_outputs_skips_when_no_out_of_place_outputs() {
    // main.tf has resources only; outputs.tf has the lone output.
    // Action should NOT appear.
    let main = uri("file:///nonexistent-mod-mo3/main.tf");
    let outputs = uri("file:///nonexistent-mod-mo3/outputs.tf");
    let backend = fresh_backend("resource \"null_resource\" \"r\" {}\n", &main);
    add_doc(&backend, &outputs, "output \"a\" { value = 1 }\n");
    let actions = all_actions_for(&backend, &main).await;
    let any_move = actions.iter().any(|a| match a {
        CodeActionOrCommand::CodeAction(ca) => ca.title.starts_with("Move "),
        _ => false,
    });
    assert!(!any_move, "no move-outputs action when nothing to move");
}

#[tokio::test]
async fn scope_kind_namespacing_matches_spec() {
    // Pin the LSP `CodeActionKind` strings the helper produces so
    // clients filtering via `context.only` keep working.
    let u = uri("file:///mod/main.tf");
    let backend = fresh_backend("output \"o\" { value = \"${var.x}\" }\nvariable \"x\" {}\n", &u);
    let actions = all_actions_for(&backend, &u).await;
    let mut seen_file = false;
    let mut seen_module = false;
    let mut seen_workspace = false;
    for a in &actions {
        let CodeActionOrCommand::CodeAction(ca) = a else { continue };
        let Some(kind) = ca.kind.as_ref() else { continue };
        if kind.as_str() == "source.fixAll.terraform-ls-rs.unwrap-interpolation" {
            seen_file = true;
        }
        if kind.as_str() == "source.fixAll.terraform-ls-rs.unwrap-interpolation.module" {
            seen_module = true;
        }
        if kind.as_str() == "source.fixAll.terraform-ls-rs.unwrap-interpolation.workspace" {
            seen_workspace = true;
        }
    }
    assert!(seen_file, "file kind emitted");
    assert!(seen_module, "module kind emitted");
    assert!(seen_workspace, "workspace kind emitted");
}

// ── format-as-code-action ──────────────────────────────────────────

#[tokio::test]
async fn format_action_file_emits_when_unformatted() {
    // Mis-aligned `=` should produce a "Format file" action.
    let u = uri("file:///fmt-mod-1/main.tf");
    let src = "resource \"x\" \"y\" {\n  ami = \"a\"\n  instance_type = \"t\"\n}\n";
    let backend = fresh_backend(src, &u);

    let actions = all_actions_for(&backend, &u).await;
    let action = find_action(&actions, "Format file");
    let edits = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .expect("file edit");
    assert_eq!(edits.len(), 1, "single whole-file TextEdit");
    assert!(
        edits[0].new_text.contains("ami           = \"a\""),
        "expected aligned output, got:\n{}",
        edits[0].new_text
    );
}

#[tokio::test]
async fn format_action_file_skipped_when_clean() {
    // Already-formatted input — Format action must NOT appear.
    let u = uri("file:///fmt-mod-2/main.tf");
    let src = "resource \"x\" \"y\" {\n  ami           = \"a\"\n  instance_type = \"t\"\n}\n";
    let backend = fresh_backend(src, &u);

    let actions = all_actions_for(&backend, &u).await;
    let any_format = actions.iter().any(|a| match a {
        CodeActionOrCommand::CodeAction(ca) => ca.title.starts_with("Format"),
        _ => false,
    });
    assert!(!any_format, "no format action when buffer already formatted");
}

#[tokio::test]
async fn format_action_module_covers_dirty_siblings_only() {
    // Three .tf in same module: clean, dirty, dirty.
    // Expect "Format 2 .tf files in this module" with edits
    // covering only the two dirty URIs.
    let a = uri("file:///fmt-mod-3/a.tf");
    let b = uri("file:///fmt-mod-3/b.tf");
    let c = uri("file:///fmt-mod-3/c.tf");
    // a.tf is already idempotent under minimal style (multi-
    // line variable block) — proves the action skips clean
    // siblings when computing the count.
    let backend = fresh_backend("variable \"a\" {\n  default = 1\n}\n", &a);
    add_doc(
        &backend,
        &b,
        "resource \"x\" \"y\" {\n  ami = \"a\"\n  instance_type = \"t\"\n}\n",
    );
    add_doc(
        &backend,
        &c,
        "resource \"x\" \"z\" {\n  ami = \"a\"\n  count = 1\n}\n",
    );

    let actions = all_actions_for(&backend, &a).await;
    let action = find_action(&actions, "Format 2 .tf files in this module");
    let changes = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .expect("changes map");
    assert_eq!(changes.len(), 2, "exactly the two dirty siblings");
    assert!(!changes.contains_key(&a), "clean a.tf untouched");
    assert!(changes.contains_key(&b), "dirty b.tf included");
    assert!(changes.contains_key(&c), "dirty c.tf included");
}

#[tokio::test]
async fn format_action_workspace_skips_clean_files() {
    // Cross-dir: one dirty, one clean. Workspace count = 1.
    let dirty = uri("file:///fmt-ws-A/main.tf");
    let clean = uri("file:///fmt-ws-B/main.tf");
    let backend = fresh_backend(
        "resource \"x\" \"y\" {\n  ami = \"a\"\n  instance_type = \"t\"\n}\n",
        &dirty,
    );
    add_doc(&backend, &clean, "variable \"x\" {\n  default = 1\n}\n");

    let actions = all_actions_for(&backend, &dirty).await;
    let action = find_action(&actions, "Format 1 .tf file in workspace");
    let changes = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert!(changes.contains_key(&dirty));
    assert!(!changes.contains_key(&clean));
}

#[tokio::test]
async fn format_action_selection_uses_range() {
    // Visual selection over two unaligned attribute lines.
    let u = uri("file:///fmt-mod-4/main.tf");
    let src = "resource \"x\" \"y\" {\n  ami = \"a\"\n  instance_type = \"t\"\n}\n";
    let backend = fresh_backend(src, &u);

    // Select lines 1..3 (inclusive) — the two attribute lines + close.
    let range = Range {
        start: Position::new(1, 0),
        end: Position::new(3, 0),
    };
    let actions = all_actions_for_selection(&backend, &u, range).await;
    let action = find_action(&actions, "Format selection");
    let edits = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .expect("selection edit");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].range, range, "edit covers exactly the selection");
}

#[tokio::test]
async fn format_action_respects_runtime_style_toggle() {
    // Source has resources in non-alphabetical order. Default
    // (minimal) format leaves order intact. Switch to
    // opinionated → format-action edit reorders them.
    use tfls_state::FormatStyle;
    let u = uri("file:///fmt-mod-5/main.tf");
    let src = concat!(
        "resource \"x\" \"z\" { ami = \"z\" }\n",
        "resource \"x\" \"a\" { ami = \"a\" }\n",
    );
    let backend = fresh_backend(src, &u);

    // Minimal: order preserved → only alignment may change.
    {
        let actions = all_actions_for(&backend, &u).await;
        if let Some(a) = actions.iter().find_map(|x| match x {
            CodeActionOrCommand::CodeAction(ca) if ca.title == "Format file" => Some(ca),
            _ => None,
        }) {
            let edits = a
                .edit
                .as_ref()
                .and_then(|e| e.changes.as_ref())
                .and_then(|c| c.get(&u))
                .unwrap();
            let z_pos = edits[0].new_text.find('z').unwrap();
            let a_pos = edits[0].new_text.find("\"a\"").unwrap();
            assert!(
                z_pos < a_pos,
                "minimal: z must precede a, got:\n{}",
                edits[0].new_text
            );
        }
    }

    // Switch to opinionated.
    let json: sonic_rs::Value =
        sonic_rs::from_str(r#"{"formatStyle":"opinionated"}"#).unwrap();
    backend.state.config.update_from_json(&json);
    assert_eq!(
        backend.state.config.snapshot().format_style,
        FormatStyle::Opinionated
    );

    let actions = all_actions_for(&backend, &u).await;
    let action = find_action(&actions, "Format file");
    let edits = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .unwrap();
    let a_pos = edits[0].new_text.find("\"a\"").unwrap();
    let z_pos = edits[0].new_text.find("\"z\"").unwrap();
    assert!(
        a_pos < z_pos,
        "opinionated: a must precede z, got:\n{}",
        edits[0].new_text
    );
}

// ── null_resource → terraform_data ──────────────────────────────

#[tokio::test]
async fn null_resource_action_instance_at_cursor() {
    let u = uri("file:///fmt-nrt-1/main.tf");
    let src = "resource \"null_resource\" \"x\" {}\n";
    let backend = fresh_backend(src, &u);

    // Cursor anywhere inside the block.
    let actions = all_actions_for(&backend, &u).await;
    let action = find_action(&actions, "Convert null_resource.x to terraform_data");
    let edits = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .expect("file edit");
    assert!(
        edits.iter().any(|e| e.new_text == "\"terraform_data\""),
        "expected label rewrite, got:\n{edits:?}"
    );
}

#[tokio::test]
async fn null_resource_action_renames_triggers_attribute() {
    let u = uri("file:///fmt-nrt-2/main.tf");
    let src = "resource \"null_resource\" \"x\" {\n  triggers = {\n    foo = \"bar\"\n  }\n}\n";
    let backend = fresh_backend(src, &u);

    let actions = all_actions_for(&backend, &u).await;
    let action = find_action(&actions, "Convert 1 null_resource block in this file");
    let edits = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .and_then(|c| c.get(&u))
        .expect("file edit");
    assert!(
        edits.iter().any(|e| e.new_text == "\"terraform_data\""),
        "label rewrite missing"
    );
    assert!(
        edits.iter().any(|e| e.new_text == "triggers_replace"),
        "triggers rename missing; got:\n{edits:?}"
    );
}

#[tokio::test]
async fn null_resource_action_module_covers_siblings() {
    let a = uri("file:///fmt-nrt-3/main.tf");
    let b = uri("file:///fmt-nrt-3/extra.tf");
    let backend = fresh_backend("resource \"null_resource\" \"a\" {}\n", &a);
    add_doc(&backend, &b, "resource \"null_resource\" \"b\" {}\n");

    let actions = all_actions_for(&backend, &a).await;
    let action = find_action(&actions, "Convert 2 null_resource blocks in this module");
    let changes = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .expect("changes map");
    assert!(changes.contains_key(&a));
    assert!(changes.contains_key(&b));
}

#[tokio::test]
async fn null_resource_action_skipped_when_none() {
    let u = uri("file:///fmt-nrt-4/main.tf");
    let src = "resource \"aws_instance\" \"x\" { ami = \"a\" }\n";
    let backend = fresh_backend(src, &u);

    let actions = all_actions_for(&backend, &u).await;
    let any_convert = actions.iter().any(|a| match a {
        CodeActionOrCommand::CodeAction(ca) => ca.title.starts_with("Convert null_resource") ||
            ca.title.starts_with("Convert 1 null_resource") ||
            ca.title.starts_with("Convert null_resource"),
        _ => false,
    });
    assert!(!any_convert, "no null_resource action when none present");
}

#[tokio::test]
async fn null_resource_action_suppressed_when_required_version_excludes_1_4() {
    // `required_version = "< 1.3"` excludes 1.4 entirely → no action.
    let u = uri("file:///fmt-nrt-5/main.tf");
    let src = concat!(
        "terraform { required_version = \"< 1.3\" }\n",
        "resource \"null_resource\" \"x\" {}\n",
    );
    let backend = fresh_backend(src, &u);
    let actions = all_actions_for(&backend, &u).await;
    let any_convert = actions.iter().any(|a| match a {
        CodeActionOrCommand::CodeAction(ca) => {
            ca.title.contains("null_resource") && ca.title.contains("terraform_data")
        }
        _ => false,
    });
    assert!(
        !any_convert,
        "version-aware gate failed; got actions:\n{actions:?}"
    );
}

#[tokio::test]
async fn null_resource_action_workspace_covers_unrelated_dirs() {
    let a = uri("file:///fmt-nrt-A/main.tf");
    let b = uri("file:///fmt-nrt-B/main.tf");
    let backend = fresh_backend("resource \"null_resource\" \"a\" {}\n", &a);
    add_doc(&backend, &b, "resource \"null_resource\" \"b\" {}\n");
    let actions = all_actions_for(&backend, &a).await;
    let action = find_action(&actions, "Convert 2 null_resource blocks in workspace");
    let changes = action
        .edit
        .as_ref()
        .and_then(|e| e.changes.as_ref())
        .expect("changes map");
    assert_eq!(changes.len(), 2);
}
