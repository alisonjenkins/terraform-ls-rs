//! Opening a `.tf` file outside the original workspace root triggers a
//! one-shot scan of its directory, so sibling definitions become visible
//! to cross-file diagnostics.
//!
//! Without this, editing a file under `/other-project/` while the LSP was
//! initialised with `/primary-workspace/` as its only workspace folder
//! produces false-positive "undefined variable" warnings for every
//! reference whose definition lives in a sibling `.tf` file.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tfls_lsp::{Backend, indexer};
use tfls_state::{JobQueue, StateStore};
use tower_lsp::LspService;
use tower_lsp::lsp_types::{DidOpenTextDocumentParams, TextDocumentItem, Url};

fn tmp_dir(label: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "tfls-mod-aware-{label}-{}-{nanos}",
        std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}

#[tokio::test]
async fn did_open_in_new_directory_indexes_sibling_files() {
    // /primary/ — initial workspace root.
    let primary = tmp_dir("primary");
    // /other/ — unrelated directory that contains the file the user is
    // about to open in the editor, plus a sibling that defines the
    // variable it references.
    let other = tmp_dir("other");
    fs::write(
        other.join("variables.tf"),
        "variable \"region\" { default = \"us-east-1\" }\n",
    )
    .unwrap();
    let main_path = other.join("main.tf");
    fs::write(&main_path, "output \"where\" { value = var.region }\n").unwrap();

    // Spin up a Backend with the primary workspace as its only root.
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );

    // Start the worker so background ScanDirectory jobs drain.
    let worker_state = Arc::clone(&inner.state);
    let worker_jobs = Arc::clone(&inner.jobs);
    let worker = indexer::spawn_worker(worker_state, worker_jobs);

    // Scan the primary workspace (mirrors what `initialize` would do).
    indexer::enqueue_workspace_scan(&inner.state, &inner.jobs, &primary);

    // Now simulate the editor opening main.tf under /other/.
    let main_uri = Url::from_file_path(&main_path).unwrap();
    let main_text = fs::read_to_string(&main_path).unwrap();
    tfls_lsp::handlers::document::did_open(
        &backend,
        DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: main_uri.clone(),
                language_id: "terraform".into(),
                version: 1,
                text: main_text,
            },
        },
    )
    .await;

    // Give the worker a moment to process the ScanDirectory job.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let vars_url = Url::from_file_path(other.join("variables.tf")).unwrap();
    while std::time::Instant::now() < deadline {
        if inner.state.documents.contains_key(&vars_url) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        inner.state.documents.contains_key(&vars_url),
        "sibling variables.tf was never parsed — scanned_dirs contains: {:?}",
        inner
            .state
            .scanned_dirs
            .iter()
            .map(|d| d.key().clone())
            .collect::<Vec<_>>()
    );

    // And now diagnostics should no longer flag var.region as undefined.
    let diags = tfls_lsp::handlers::document::compute_diagnostics(&inner.state, &main_uri);
    let messages: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();
    assert!(
        messages.iter().all(|m| !m.contains("undefined variable")),
        "unexpected undefined-variable warning after sibling indexing: {messages:?}",
    );

    worker.abort();
    let _ = fs::remove_dir_all(&primary);
    let _ = fs::remove_dir_all(&other);
}

#[tokio::test]
async fn second_open_in_same_directory_does_not_rescan() {
    // Opening two files in the same directory should result in exactly
    // one ScanDirectory job (idempotency via `scanned_dirs`).
    let dir = tmp_dir("dedupe");
    let a = dir.join("a.tf");
    let b = dir.join("b.tf");
    fs::write(&a, "variable \"x\" {}\n").unwrap();
    fs::write(&b, "output \"y\" { value = var.x }\n").unwrap();

    let state = Arc::new(StateStore::new());
    let jobs = Arc::new(JobQueue::new());

    let uri_a = Url::from_file_path(&a).unwrap();
    let uri_b = Url::from_file_path(&b).unwrap();

    indexer::ensure_module_indexed(&state, &jobs, &uri_a);
    let after_first = jobs.len();
    indexer::ensure_module_indexed(&state, &jobs, &uri_b);
    let after_second = jobs.len();

    assert_eq!(
        after_first, 1,
        "first open should enqueue exactly one ScanDirectory job"
    );
    assert_eq!(
        after_second, 1,
        "second open in the same dir should not enqueue again"
    );

    let _ = fs::remove_dir_all(&dir);
}
