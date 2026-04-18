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
