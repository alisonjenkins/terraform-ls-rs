//! Integration tests for `module "…" { … }` smart handling —
//! input-variable completion, output completion, and hover on input
//! names.
//!
//! Because the resolver canonicalises paths, these tests materialise a
//! real directory tree under `std::env::temp_dir()` so local-source
//! resolution works.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    CompletionContext, CompletionParams, CompletionResponse, CompletionTriggerKind,
    HoverParams, PartialResultParams, Position, TextDocumentIdentifier,
    TextDocumentPositionParams, Url, WorkDoneProgressParams,
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn make_tmp_tree(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "tfls-module-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn fresh_backend() -> Backend {
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    )
}

fn insert_doc(backend: &Backend, path: &Path, text: &str) -> Url {
    // Ensure the containing dir exists so canonicalize succeeds.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    // Materialise an empty file so `Url::from_file_path` round-trips
    // cleanly (some OSes only accept URIs for existing paths).
    if !path.exists() {
        fs::write(path, "").unwrap();
    }
    let uri = Url::from_file_path(path).unwrap();
    backend
        .state
        .upsert_document(DocumentState::new(uri.clone(), text, 1));
    uri
}

fn make_completion_params(uri: &Url, pos: Position) -> CompletionParams {
    CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
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

fn make_hover_params(uri: &Url, pos: Position) -> HoverParams {
    HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position: pos,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    }
}

fn labels(resp: CompletionResponse) -> Vec<String> {
    match resp {
        CompletionResponse::Array(items) => items.into_iter().map(|i| i.label).collect(),
        CompletionResponse::List(list) => list.items.into_iter().map(|i| i.label).collect(),
    }
}

/// Extract `"|"` cursor marker from `marked`, returning the clean
/// source and the LSP position of the cursor.
fn src_with_cursor(marked: &str) -> (String, Position) {
    const MARKER: &str = "|";
    let idx = marked.find(MARKER).expect("missing cursor marker");
    let cleaned = format!("{}{}", &marked[..idx], &marked[idx + MARKER.len()..]);
    let before = &marked[..idx];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = (idx - line_start) as u32;
    (cleaned, Position::new(line, col))
}

/// Regression: when the active dir was already marked "scanned" (via
/// an earlier workspace scan) and then the user opens a file in it
/// via `did_open`, the child-module scan *must* still be triggered.
/// Previously `ensure_module_indexed` short-circuited on the first
/// `dir_scans` hit and skipped child discovery.
#[tokio::test]
async fn ensure_module_indexed_triggers_child_scans_even_when_dir_already_scanned() {
    use std::sync::Arc;
    use tokio::time::{Duration, sleep};

    let tree = make_tmp_tree("already-scanned");
    let root_main = tree.join("main.tf");
    let child_vars = tree.join("mod").join("vars.tf");
    fs::create_dir_all(child_vars.parent().unwrap()).unwrap();
    fs::write(
        &child_vars,
        "variable \"region\" { type = string }\n",
    )
    .unwrap();
    fs::write(
        &root_main,
        "module \"web\" { source = \"./mod\" }\n",
    )
    .unwrap();

    // Spin up a real backend + worker so Job::ScanDirectory actually
    // processes.
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let _backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );
    let worker = tfls_lsp::indexer::spawn_worker(
        Arc::clone(&inner.state),
        Arc::clone(&inner.jobs),
        None,
    );

    // Simulate the workspace-scan pre-population: mark the dir as
    // Completed without enqueueing a ScanDirectory job. A `Completed`
    // entry is the state a finished bulk scan would leave behind.
    inner.state.mark_scan_completed(tree.clone());
    inner.state.upsert_document(DocumentState::new(
        Url::from_file_path(&root_main).unwrap(),
        &fs::read_to_string(&root_main).unwrap(),
        1,
    ));

    // Now simulate did_open on main.tf.
    let main_uri = Url::from_file_path(&root_main).unwrap();
    tfls_lsp::indexer::ensure_module_indexed(&inner.state, &inner.jobs, &main_uri);

    // Give the worker a moment to process the child scan.
    sleep(Duration::from_millis(400)).await;

    // The child's `variable "region"` should now be in the store.
    let child_uri = Url::from_file_path(&child_vars).unwrap();
    let found = inner.state.documents.contains_key(&child_uri);
    worker.abort();
    assert!(found, "child module was not indexed despite ensure_module_indexed");
}

#[tokio::test]
async fn module_body_suggests_child_input_variables() {
    let tree = make_tmp_tree("inputs");
    let root_main = tree.join("main.tf");
    let child_vars = tree.join("mod").join("vars.tf");

    let (main_src, pos) = src_with_cursor(
        "module \"web\" {\n  source = \"./mod\"\n  |\n}\n",
    );
    let backend = fresh_backend();
    let _child_uri = insert_doc(
        &backend,
        &child_vars,
        "variable \"region\" { type = string }\nvariable \"name\" {}\n",
    );
    let main_uri = insert_doc(&backend, &root_main, &main_src);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_completion_params(&main_uri, pos),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"region".to_string()), "got: {ls:?}");
    assert!(ls.contains(&"name".to_string()), "got: {ls:?}");
}

#[tokio::test]
async fn module_body_filters_already_set_inputs() {
    let tree = make_tmp_tree("filter");
    let root_main = tree.join("main.tf");
    let child_vars = tree.join("mod").join("vars.tf");

    let (main_src, pos) = src_with_cursor(
        "module \"web\" {\n  source = \"./mod\"\n  region = \"us-east-1\"\n  |\n}\n",
    );
    let backend = fresh_backend();
    let _ = insert_doc(
        &backend,
        &child_vars,
        "variable \"region\" {}\nvariable \"name\" {}\n",
    );
    let main_uri = insert_doc(&backend, &root_main, &main_src);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_completion_params(&main_uri, pos),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(!ls.contains(&"region".to_string()), "region already set: {ls:?}");
    assert!(ls.contains(&"name".to_string()), "got: {ls:?}");
}

#[tokio::test]
async fn module_attr_suggests_outputs() {
    let tree = make_tmp_tree("outputs");
    let root_main = tree.join("main.tf");
    let child_outs = tree.join("mod").join("outs.tf");

    let (main_src, pos) = src_with_cursor(
        "module \"web\" { source = \"./mod\" }\noutput \"x\" { value = module.web.|xxx }\n",
    );
    let backend = fresh_backend();
    let _ = insert_doc(
        &backend,
        &child_outs,
        "output \"url\" { value = \"x\" }\noutput \"arn\" { value = \"y\" }\n",
    );
    let main_uri = insert_doc(&backend, &root_main, &main_src);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_completion_params(&main_uri, pos),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert_eq!(ls, vec!["arn".to_string(), "url".to_string()]);
}

#[tokio::test]
async fn module_attr_returns_empty_for_unknown_module() {
    let tree = make_tmp_tree("unknown");
    let root_main = tree.join("main.tf");

    let (main_src, pos) = src_with_cursor(
        "output \"x\" { value = module.web.|xxx }\n",
    );
    let backend = fresh_backend();
    let main_uri = insert_doc(&backend, &root_main, &main_src);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_completion_params(&main_uri, pos),
    )
    .await
    .expect("ok");
    assert!(resp.is_none(), "no such module; got {resp:?}");
}

#[tokio::test]
async fn module_body_empty_for_remote_source_without_lockfile() {
    let tree = make_tmp_tree("remote-no-lock");
    let root_main = tree.join("main.tf");

    let (main_src, pos) = src_with_cursor(
        "module \"web\" {\n  source = \"hashicorp/vpc/aws\"\n  |\n}\n",
    );
    let backend = fresh_backend();
    let main_uri = insert_doc(&backend, &root_main, &main_src);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_completion_params(&main_uri, pos),
    )
    .await
    .expect("ok");
    assert!(resp.is_none(), "no lockfile → no suggestions; got {resp:?}");
}

#[tokio::test]
async fn module_body_uses_lockfile_for_remote_source() {
    let tree = make_tmp_tree("remote-lock");
    let root_main = tree.join("main.tf");
    let cached = tree.join("modules").join("web");
    let lockdir = tree.join(".terraform").join("modules");
    fs::create_dir_all(&cached).unwrap();
    fs::create_dir_all(&lockdir).unwrap();
    fs::write(
        lockdir.join("modules.json"),
        r#"{"Modules":[{"Key":"web","Source":"x","Dir":"modules/web"}]}"#,
    )
    .unwrap();
    let child_vars = cached.join("variables.tf");

    let (main_src, pos) = src_with_cursor(
        "module \"web\" {\n  source = \"hashicorp/example/aws\"\n  |\n}\n",
    );
    let backend = fresh_backend();
    let _ = insert_doc(
        &backend,
        &child_vars,
        "variable \"region\" { type = string }\n",
    );
    let main_uri = insert_doc(&backend, &root_main, &main_src);

    let resp = tfls_lsp::handlers::completion::completion(
        &backend,
        make_completion_params(&main_uri, pos),
    )
    .await
    .expect("ok")
    .expect("some completions");
    let ls = labels(resp);
    assert!(ls.contains(&"region".to_string()), "got: {ls:?}");
}

#[tokio::test]
async fn hover_on_module_input_renders_description_and_type() {
    let tree = make_tmp_tree("hover");
    let root_main = tree.join("main.tf");
    let child_vars = tree.join("mod").join("vars.tf");

    // `region` is at line 2, column 2.
    let main_src = "module \"web\" {\n  source = \"./mod\"\n  region = \"us-east-1\"\n}\n";
    let backend = fresh_backend();
    let _ = insert_doc(
        &backend,
        &child_vars,
        "variable \"region\" {\n  type = string\n  description = \"AWS region\"\n}\n",
    );
    let main_uri = insert_doc(&backend, &root_main, main_src);

    let resp = tfls_lsp::handlers::navigation::hover(
        &backend,
        make_hover_params(&main_uri, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some hover");
    let tower_lsp::lsp_types::HoverContents::Markup(content) = resp.contents else {
        panic!("expected markup hover");
    };
    assert!(content.value.contains("region"), "got: {}", content.value);
    assert!(content.value.contains("string"), "got: {}", content.value);
    assert!(content.value.contains("AWS region"), "got: {}", content.value);
}

#[tokio::test]
async fn hover_on_module_input_without_description_still_shows_type() {
    let tree = make_tmp_tree("hover-no-desc");
    let root_main = tree.join("main.tf");
    let child_vars = tree.join("mod").join("vars.tf");

    let main_src = "module \"web\" {\n  source = \"./mod\"\n  region = \"us-east-1\"\n}\n";
    let backend = fresh_backend();
    let _ = insert_doc(
        &backend,
        &child_vars,
        "variable \"region\" { type = string }\n",
    );
    let main_uri = insert_doc(&backend, &root_main, main_src);

    let resp = tfls_lsp::handlers::navigation::hover(
        &backend,
        make_hover_params(&main_uri, Position::new(2, 4)),
    )
    .await
    .expect("ok")
    .expect("some hover");
    let tower_lsp::lsp_types::HoverContents::Markup(content) = resp.contents else {
        panic!("expected markup hover");
    };
    assert!(content.value.contains("region"));
    assert!(content.value.contains("string"));
}

#[tokio::test]
async fn hover_on_unknown_module_input_returns_none() {
    let tree = make_tmp_tree("hover-unknown");
    let root_main = tree.join("main.tf");
    let child_vars = tree.join("mod").join("vars.tf");

    let main_src = "module \"web\" {\n  source = \"./mod\"\n  bogus = \"x\"\n}\n";
    let backend = fresh_backend();
    let _ = insert_doc(
        &backend,
        &child_vars,
        "variable \"region\" { type = string }\n",
    );
    let main_uri = insert_doc(&backend, &root_main, main_src);

    // The fallback symbol hover may still produce a response for the
    // enclosing module label — we just assert it isn't specifically
    // the module-input hover's "#### … (*type*)" format with a
    // variable name the child doesn't declare.
    let resp = tfls_lsp::handlers::navigation::hover(
        &backend,
        make_hover_params(&main_uri, Position::new(2, 4)),
    )
    .await
    .expect("ok");
    if let Some(hover) = resp {
        if let tower_lsp::lsp_types::HoverContents::Markup(content) = hover.contents {
            assert!(
                !content.value.contains("### `bogus`"),
                "module-input hover fired for unknown variable: {}",
                content.value
            );
        }
    }
}

#[tokio::test]
async fn hover_on_module_output_reference_renders_description() {
    // `module.web.endpoint` — hover on `endpoint` should pull the
    // `description` from the child module's `output "endpoint" { }`.
    let tree = make_tmp_tree("hover-output-ref");
    let child_outputs = tree.join("mod").join("outputs.tf");
    let child_vars = tree.join("mod").join("variables.tf");

    let backend = fresh_backend();
    let _ = insert_doc(
        &backend,
        &child_outputs,
        "output \"endpoint\" {\n  value = \"\"\n  description = \"HTTPS endpoint the service listens on.\"\n}\n",
    );
    let _ = insert_doc(&backend, &child_vars, "variable \"region\" {}\n");

    // The module call lives in a peer file — the reference is in
    // another. This mirrors real stacks where `modules.tf` holds the
    // call and consumers live elsewhere.
    let _ = insert_doc(
        &backend,
        &tree.join("modules.tf"),
        "module \"web\" {\n  source = \"./mod\"\n}\n",
    );
    let ref_uri = insert_doc(
        &backend,
        &tree.join("outputs.tf"),
        "output \"api\" { value = module.web.endpoint }\n",
    );

    let resp = tfls_lsp::handlers::navigation::hover(
        &backend,
        // Cursor on `d` in `endpoint` (col 35).
        make_hover_params(&ref_uri, Position::new(0, 35)),
    )
    .await
    .expect("ok")
    .expect("some hover");
    let tower_lsp::lsp_types::HoverContents::Markup(content) = resp.contents else {
        panic!("expected markup hover");
    };
    assert!(
        content.value.contains("module.web.endpoint"),
        "got: {}",
        content.value
    );
    assert!(
        content.value.contains("HTTPS endpoint"),
        "got: {}",
        content.value
    );
}

#[tokio::test]
async fn hover_on_module_block_header_label_renders_overview() {
    // Cursor on `"web"` in `module "web" {}` — render an overview
    // listing inputs + outputs.
    let tree = make_tmp_tree("hover-overview-block");
    let backend = fresh_backend();
    insert_doc(
        &backend,
        &tree.join("mod").join("variables.tf"),
        "variable \"region\" {\n  type = string\n  description = \"AWS region\"\n}\nvariable \"count\" {\n  type = number\n  default = 1\n  description = \"How many nodes.\"\n}\n",
    );
    insert_doc(
        &backend,
        &tree.join("mod").join("outputs.tf"),
        "output \"endpoint\" {\n  value = \"\"\n  description = \"Public URL.\"\n}\n",
    );
    let main_uri = insert_doc(
        &backend,
        &tree.join("main.tf"),
        "module \"web\" {\n  source = \"./mod\"\n}\n",
    );

    let resp = tfls_lsp::handlers::navigation::hover(
        &backend,
        // Cursor on `w` in the label `"web"` (line 0, col 9).
        make_hover_params(&main_uri, Position::new(0, 9)),
    )
    .await
    .expect("ok")
    .expect("some hover");
    let tower_lsp::lsp_types::HoverContents::Markup(content) = resp.contents else {
        panic!("expected markup hover");
    };
    // Overview must list inputs + outputs with types and descriptions.
    assert!(content.value.contains("module.web"), "label in: {}", content.value);
    assert!(content.value.contains("#### Inputs"), "header in: {}", content.value);
    assert!(content.value.contains("#### Outputs"), "header in: {}", content.value);
    assert!(content.value.contains("region"), "region input: {}", content.value);
    assert!(content.value.contains("AWS region"), "region desc: {}", content.value);
    assert!(content.value.contains("required"), "required tag: {}", content.value);
    assert!(content.value.contains("endpoint"), "output: {}", content.value);
    assert!(content.value.contains("Public URL"), "output desc: {}", content.value);
}

#[tokio::test]
async fn hover_on_module_reference_label_renders_overview() {
    // Cursor on `web` in `module.web.endpoint` — same overview as
    // the block-header case, sourced from the peer file's `module
    // "web" {}` call.
    let tree = make_tmp_tree("hover-overview-ref");
    let backend = fresh_backend();
    insert_doc(
        &backend,
        &tree.join("mod").join("variables.tf"),
        "variable \"region\" { type = string }\n",
    );
    insert_doc(
        &backend,
        &tree.join("mod").join("outputs.tf"),
        "output \"endpoint\" { value = \"\" }\n",
    );
    insert_doc(
        &backend,
        &tree.join("modules.tf"),
        "module \"web\" {\n  source = \"./mod\"\n}\n",
    );
    let ref_uri = insert_doc(
        &backend,
        &tree.join("outputs.tf"),
        "output \"api\" { value = module.web.endpoint }\n",
    );

    let resp = tfls_lsp::handlers::navigation::hover(
        &backend,
        // Cursor on the `web` label — col 30 (inside `web`).
        make_hover_params(&ref_uri, Position::new(0, 30)),
    )
    .await
    .expect("ok")
    .expect("some hover");
    let tower_lsp::lsp_types::HoverContents::Markup(content) = resp.contents else {
        panic!("expected markup hover");
    };
    assert!(content.value.contains("module.web"), "got: {}", content.value);
    assert!(content.value.contains("region"), "inputs listed: {}", content.value);
    assert!(content.value.contains("endpoint"), "outputs listed: {}", content.value);
}
