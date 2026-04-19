//! Workspace-level integration tests that exercise the background
//! indexer against a real tmp directory of `.tf` files.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tfls_lsp::indexer;
use tfls_state::{JobQueue, StateStore, SymbolKey};
use tower_lsp::lsp_types::Url;

fn tmp_dir(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tfls-workspace-{suffix}-{}-{}",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir");
    dir
}

#[tokio::test]
async fn indexer_resolves_definition_in_another_file() {
    let dir = tmp_dir("cross-file");

    // File A defines the variable.
    fs::write(
        dir.join("vars.tf"),
        "variable \"region\" { default = \"us-east-1\" }\n",
    )
    .unwrap();
    // File B references it.
    fs::write(
        dir.join("main.tf"),
        "output \"where\" { value = var.region }\n",
    )
    .unwrap();

    let state = Arc::new(StateStore::new());
    let jobs = Arc::new(JobQueue::new());
    let worker = indexer::spawn_worker(Arc::clone(&state), Arc::clone(&jobs), None);
    indexer::enqueue_workspace_scan(&state, &jobs, &dir);

    // Give the worker time to drain.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !jobs.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(jobs.len(), 0, "worker should have drained all jobs");

    // Variable defined in vars.tf should be indexed globally.
    let key = SymbolKey::new(tfls_core::SymbolKind::Variable, "region");
    let defs = state
        .definitions_by_name
        .get(&key)
        .expect("variable indexed globally");
    assert_eq!(defs.len(), 1);
    let def_uri = defs[0].uri.clone();
    assert!(def_uri.as_str().ends_with("vars.tf"), "got {def_uri}");
    drop(defs);

    // Reference from main.tf should be recorded in the cross-file index.
    let refs = state
        .references_by_name
        .get(&key)
        .expect("reference indexed globally");
    assert_eq!(refs.len(), 1);
    let ref_uri = refs[0].uri.clone();
    assert!(ref_uri.as_str().ends_with("main.tf"), "got {ref_uri}");

    worker.abort();
    fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn parse_file_job_skips_open_documents() {
    let dir = tmp_dir("skip-open");
    let path = dir.join("main.tf");
    fs::write(&path, "variable \"a\" {}\n").unwrap();

    let state = Arc::new(StateStore::new());
    let jobs = Arc::new(JobQueue::new());

    // Simulate the editor having this file open with different contents.
    let url = Url::from_file_path(&path).expect("url");
    state.upsert_document(tfls_state::DocumentState::new(
        url.clone(),
        "variable \"editor_only\" {}\n",
        42,
    ));

    let worker = indexer::spawn_worker(Arc::clone(&state), Arc::clone(&jobs), None);
    indexer::enqueue_workspace_scan(&state, &jobs, &dir);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !jobs.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let doc = state.documents.get(&url).expect("doc present");
    assert_eq!(doc.version, 42, "indexer must not overwrite open docs");
    assert!(doc.symbols.variables.contains_key("editor_only"));
    assert!(!doc.symbols.variables.contains_key("a"));

    worker.abort();
    fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn schema_fetch_job_reports_failure_when_cli_missing() {
    // Force a CLI path that definitely won't resolve.
    let state = Arc::new(StateStore::new());
    let jobs = Arc::new(JobQueue::new());
    let worker = indexer::spawn_worker(Arc::clone(&state), Arc::clone(&jobs), None);

    jobs.enqueue(
        tfls_state::Job::FetchSchemas {
            working_dir: std::env::temp_dir(),
        },
        tfls_state::Priority::Normal,
    );

    // Wait briefly. Even on failure, the job should be dequeued.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !jobs.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(jobs.len(), 0);
    // No schemas should have been installed.
    assert_eq!(state.schemas.len(), 0);

    worker.abort();
}
