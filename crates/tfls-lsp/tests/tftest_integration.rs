//! End-to-end behaviour for Terraform test files (`.tftest.hcl`):
//! references resolve against the module under test, structural
//! diagnostics fire, the test file never pollutes the module index, and
//! completion offers the test grammar (`run`) rather than `.tf` blocks.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use lsp_types::{
    CompletionContext, CompletionParams, CompletionResponse, CompletionTriggerKind,
    PartialResultParams, Position, TextDocumentIdentifier, TextDocumentPositionParams,
    WorkDoneProgressParams,
};
use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp_server::LspService;
use url::Url;

/// A backend pre-loaded with a small module under test:
/// - `mod/variables.tf` declares `variable "region"`.
/// - `mod/outputs.tf` declares `output "id"`.
///
/// The caller adds the test file (and optionally `mod/main.tf`).
fn backend_with_module() -> Backend {
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let upsert = |path: &str, src: &str| {
        let u = Url::parse(path).expect("url");
        inner.state.upsert_document(DocumentState::new(u, src, 1));
    };
    upsert(
        "file:///mod/variables.tf",
        "variable \"region\" {\n  type = string\n}\n",
    );
    upsert(
        "file:///mod/outputs.tf",
        "output \"id\" {\n  value = 1\n}\n",
    );
    Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    )
}

fn upsert_doc(backend: &Backend, path: &str, src: &str) -> Url {
    let u = Url::parse(path).expect("url");
    backend
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));
    u
}

fn make_params(u: &Url, pos: Position) -> CompletionParams {
    CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: tfls_core::uri::url_to_uri(u),
            },
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
async fn test_file_reference_resolves_against_module_under_test() {
    let backend = backend_with_module();
    // tests/ subdir → module under test is the parent `mod/` dir.
    let test_uri = upsert_doc(
        &backend,
        "file:///mod/tests/main.tftest.hcl",
        "run \"x\" {\n  assert {\n    condition     = var.region != \"\"\n    error_message = \"region required\"\n  }\n}\n",
    );
    let diags = tfls_lsp::handlers::document::compute_diagnostics(&backend.state, &test_uri);
    let messages: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();
    assert!(
        messages.iter().all(|m| !m.contains("undefined")),
        "var.region must resolve against the module under test: {messages:?}"
    );
}

#[tokio::test]
async fn structural_validator_flags_bad_command() {
    let backend = backend_with_module();
    let test_uri = upsert_doc(
        &backend,
        "file:///mod/tests/main.tftest.hcl",
        "run \"x\" {\n  command = \"destroy\"\n}\n",
    );
    let diags = tfls_lsp::handlers::document::compute_diagnostics(&backend.state, &test_uri);
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("invalid `command`")),
        "bad command enum must be flagged: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn module_only_rules_do_not_fire_on_test_file() {
    // A bare `variable "x" {}` in a `.tf` file would draw an
    // "unused declaration" warning; in a test file the module ruleset is
    // gated off entirely.
    let backend = backend_with_module();
    let test_uri = upsert_doc(
        &backend,
        "file:///mod/tests/main.tftest.hcl",
        "variables {\n  region = \"eu\"\n}\nrun \"x\" {\n  command = plan\n}\n",
    );
    let diags = tfls_lsp::handlers::document::compute_diagnostics(&backend.state, &test_uri);
    assert!(
        diags.is_empty(),
        "a clean test file should produce no diagnostics: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn test_file_does_not_pollute_module_index() {
    let backend = backend_with_module();
    // The test file declares a `variable` that exists ONLY here.
    upsert_doc(
        &backend,
        "file:///mod/tests/main.tftest.hcl",
        "variable \"tftest_only\" {}\nrun \"x\" {}\n",
    );
    // A sibling module file references that name — it must stay undefined,
    // proving the test file's declaration never entered the index.
    let main_uri = upsert_doc(
        &backend,
        "file:///mod/main.tf",
        "output \"o\" {\n  value = var.tftest_only\n}\n",
    );
    let diags = tfls_lsp::handlers::document::compute_diagnostics(&backend.state, &main_uri);
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("undefined") && d.message.contains("tftest_only")),
        "test-file variable must NOT resolve module references: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn completion_offers_run_in_test_file_not_resource() {
    let backend = backend_with_module();
    let test_uri = upsert_doc(&backend, "file:///mod/tests/main.tftest.hcl", "");
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&test_uri, Position::new(0, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"run".to_string()), "got: {ls:?}");
    assert!(ls.contains(&"mock_provider".to_string()), "got: {ls:?}");
    assert!(!ls.contains(&"resource".to_string()), "got: {ls:?}");
}

#[tokio::test]
async fn completion_offers_resource_in_tf_file_not_run() {
    let backend = backend_with_module();
    let main_uri = upsert_doc(&backend, "file:///mod/main.tf", "");
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&main_uri, Position::new(0, 0)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"resource".to_string()), "got: {ls:?}");
    assert!(!ls.contains(&"run".to_string()), "got: {ls:?}");
}

#[tokio::test]
async fn completion_offers_module_outputs_after_output_dot() {
    let backend = backend_with_module();
    // Cursor right after `output.` in an assert condition.
    let src =
        "run \"x\" {\n  assert {\n    condition     = output.\n    error_message = \"m\"\n  }\n}\n";
    let test_uri = upsert_doc(&backend, "file:///mod/tests/main.tftest.hcl", src);
    // Position on the `output.` line (line 2), just past the dot.
    let line = "    condition     = output.";
    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_params(&test_uri, Position::new(2, line.len() as u32)),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(
        ls.contains(&"id".to_string()),
        "module output `id` must be offered: {ls:?}"
    );
}
