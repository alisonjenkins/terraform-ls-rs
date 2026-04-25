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
async fn code_action_no_action_when_default_resolves_to_any() {
    // Reference defaults can't be statically typed.
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
    assert!(
        actions.is_empty(),
        "must not offer action for unresolvable default; got {actions:?}"
    );
}

#[tokio::test]
async fn code_action_no_action_for_empty_array_default() {
    // `default = []` is too ambiguous (could be list/set of any
    // primitive). Refuse rather than guess.
    let u = uri("file:///vars.tf");
    let src = concat!(
        "variable \"items\" {\n",
        "  default = []\n",
        "}\n",
        "output \"i\" { value = var.items }\n",
    );
    let backend = fresh_backend(src, &u);

    let actions = code_actions_for(&backend, &u, "variable has no type").await;
    assert!(
        actions.is_empty(),
        "empty array is too ambiguous; got {actions:?}"
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
async fn code_action_skips_when_assigned_types_disagree() {
    // Two callers / tfvars files give DIFFERENT types for the same
    // variable → no canonical answer → no action.
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
    assert!(
        actions.is_empty(),
        "disagreement must skip the action; got {actions:?}"
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
    assert!(
        actions.is_empty(),
        "must not offer action when block already has type; got {actions:?}"
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
