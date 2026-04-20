//! Background workspace indexer: consumes jobs from the state's job
//! queue and keeps the store in sync with disk.
//!
//! Runs as spawned tokio tasks owned by the [`Backend`]. Cancellable
//! by aborting the task handles at shutdown.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tfls_schema::{SchemaError, SchemaFetcher, functions_cache};
use tfls_state::{DocumentState, Job, JobQueue, Priority, StateStore};
use tfls_walker::{
    WalkerError, WorkspaceEvent, discover_terraform_files, discover_terraform_files_in_dir,
    watch_workspace,
};
use thiserror::Error;

const WATCH_DEBOUNCE_MS: u64 = 150;
const SCHEMA_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum IndexerError {
    #[error("failed to read '{path}'")]
    FileRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to fetch provider schemas")]
    SchemaFetch(#[source] SchemaError),
}

/// Spawn the worker loop. The returned handle can be aborted at
/// shutdown to stop draining the queue.
///
/// `client` is used to publish diagnostics after each `ParseFile`
/// job so Trouble / other LSP consumers see diagnostics for every
/// indexed workspace file, not just files currently in a buffer.
/// Neovim doesn't auto-pull `workspace/diagnostic` even when the
/// server advertises it, so proactive push is the only reliable way
/// to populate `vim.diagnostic` across the whole module.
pub fn spawn_worker(
    state: Arc<StateStore>,
    queue: Arc<JobQueue>,
    client: Option<tower_lsp::Client>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let job = queue.next().await;
            if let Err(e) = handle_job(&state, &queue, client.as_ref(), job).await {
                tracing::warn!(error = %e, "background job failed");
            }
        }
    })
}

/// Enqueue a single bulk-scan job for the whole workspace. Replaces
/// the old fan-out of one `ParseFile` job per file, which flooded
/// the worker's serial queue. The bulk scan parallelises file read,
/// parse, symbol extract, and diagnostic compute with rayon and
/// dispatches publishes concurrently.
pub fn enqueue_workspace_scan(state: &StateStore, queue: &JobQueue, root: &Path) {
    // Pre-mark every scanned dir so later `did_open` events don't
    // re-enqueue `ScanDirectory` jobs for the same dirs.
    if let Ok(files) = discover_terraform_files(root) {
        tracing::info!(count = files.len(), root = %root.display(), "workspace scan (bulk)");
        for path in &files {
            if let Some(parent) = path.parent() {
                state.scanned_dirs.insert(parent.to_path_buf());
            }
        }
    }
    queue.enqueue(Job::BulkWorkspaceScan(root.to_path_buf()), Priority::Low);
}

/// Enqueue a scan for the single directory containing `file_uri` if it
/// hasn't been scanned yet. Idempotent — repeated `did_open` events for
/// the same directory trigger at most one scan. Used so opening a file
/// outside the primary workspace folder still indexes its sibling `.tf`
/// files (needed for cross-file undefined-reference resolution).
pub fn ensure_module_indexed(state: &StateStore, queue: &JobQueue, file_uri: &lsp_types::Url) {
    let Ok(path) = file_uri.to_file_path() else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    let dir_buf = dir.to_path_buf();

    // Also look for a parent with `.terraform/providers/` and enqueue a
    // schema fetch there. That's where provider schemas live, and it's
    // often not the same directory as the opened file (sub-modules
    // inherit their parent's initialisation).
    if let Some(init_root) = find_terraform_init_root(&dir_buf) {
        if state.fetched_schema_dirs.insert(init_root.clone()) {
            queue.enqueue(
                Job::FetchSchemas {
                    working_dir: init_root,
                },
                Priority::Normal,
            );
        }
    }

    // Enqueue a sibling-dir scan if this dir hasn't been scanned yet.
    // The workspace scanner pre-populates `scanned_dirs`, so a later
    // did_open may find the dir already marked — that's fine, the
    // workspace scan covered the same files. But we still need to
    // discover the module blocks they declare so child modules get
    // indexed.
    if state.scanned_dirs.insert(dir_buf.clone()) {
        queue.enqueue(Job::ScanDirectory(dir_buf.clone()), Priority::Normal);
    }
    enqueue_child_module_scans(state, queue, &dir_buf);
}

/// Enqueue a one-shot schema fetch for `root` at normal priority.
/// The worker will invoke the terraform/opentofu CLI; failure is
/// logged but doesn't abort the server (schemas are opportunistic).
pub fn enqueue_schema_fetch(queue: &JobQueue, root: &Path) {
    queue.enqueue(
        Job::FetchSchemas {
            working_dir: root.to_path_buf(),
        },
        Priority::Normal,
    );
}

/// Enqueue a one-shot functions-metadata fetch. Resolves the CLI
/// binary from PATH (preferring `tofu`, falling back to `terraform`)
/// — callers don't need to know which is installed.
pub fn enqueue_functions_fetch(queue: &JobQueue) {
    let binary = resolve_cli_binary();
    queue.enqueue(Job::FetchFunctions { binary }, Priority::Normal);
}

fn resolve_cli_binary() -> PathBuf {
    if let Ok(path) = which_binary("tofu") {
        return path;
    }
    if let Ok(path) = which_binary("terraform") {
        return path;
    }
    // Neither available — return the bare name so the fetch fails
    // predictably and the bundled snapshot is used.
    PathBuf::from("tofu")
}

fn which_binary(name: &str) -> Result<PathBuf, std::io::Error> {
    // Minimal PATH search: no need for the `which` crate.
    let path = std::env::var_os("PATH")
        .ok_or_else(|| std::io::Error::other("PATH not set"))?;
    for entry in std::env::split_paths(&path) {
        let candidate = entry.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(std::io::ErrorKind::NotFound.into())
}

/// Start a file watcher and forward change events as jobs on the queue.
pub fn spawn_watcher(
    state: Arc<StateStore>,
    queue: Arc<JobQueue>,
    root: PathBuf,
) -> Result<tokio::task::JoinHandle<()>, WalkerError> {
    let mut watcher = watch_workspace(
        &root,
        Duration::from_millis(WATCH_DEBOUNCE_MS),
    )?;

    let handle = tokio::spawn(async move {
        while let Some(event) = watcher.events.recv().await {
            match event {
                WorkspaceEvent::FileChanged(path) => {
                    queue.enqueue(Job::ParseFile(path), Priority::Normal);
                }
                WorkspaceEvent::FileRemoved(path) => {
                    if let Some(url) = path_to_url(&path) {
                        state.remove_document(&url);
                    }
                }
            }
        }
    });

    Ok(handle)
}

async fn handle_job(
    state: &StateStore,
    queue: &JobQueue,
    client: Option<&tower_lsp::Client>,
    job: Job,
) -> Result<(), IndexerError> {
    match job {
        Job::ParseFile(path) => {
            let result = parse_file_into_state(state, &path).await;
            if let Some(c) = client {
                publish_for_path(state, c, &path).await;
            }
            result
        }
        Job::ReparseDocument(url) => {
            state.reparse_document(&url);
            if let Some(c) = client {
                let diagnostics = crate::handlers::document::compute_diagnostics(state, &url);
                let version = state.documents.get(&url).map(|d| d.version);
                c.publish_diagnostics(url, diagnostics, version).await;
            }
            Ok(())
        }
        Job::FetchSchemas { working_dir } => {
            let progress = match client {
                Some(c) => {
                    crate::progress::ProgressReporter::begin(c, "Fetching provider schemas").await
                }
                None => None,
            };
            let result = fetch_and_install_schemas(state, &working_dir).await;
            if let Some(p) = progress {
                let message = match &result {
                    Ok(_) => Some("schemas ready".to_string()),
                    Err(_) => Some("schema fetch failed".to_string()),
                };
                p.end(message).await;
            }
            result
        }
        Job::FetchFunctions { binary } => {
            let progress = match client {
                Some(c) => {
                    crate::progress::ProgressReporter::begin(c, "Loading function signatures").await
                }
                None => None,
            };
            let schema = functions_cache::load_functions(&binary).await;
            let count = schema.function_signatures.len();
            state.install_functions(schema);
            tracing::info!(count, "installed function signatures");
            if let Some(p) = progress {
                p.end(Some(format!("{count} signatures"))).await;
            }
            Ok(())
        }
        Job::ScanDirectory(dir) => scan_dir_into_state(state, queue, client, &dir).await,
        Job::BulkWorkspaceScan(root) => bulk_workspace_scan(state, client, &root).await,
    }
}

/// Parallel recursive workspace scan. Discovers every `.tf` /
/// `.tf.json` under `root` and hands off to [`scan_files_parallel`]
/// for the common rayon + concurrent-publish pipeline.
async fn bulk_workspace_scan(
    state: &StateStore,
    client: Option<&tower_lsp::Client>,
    root: &Path,
) -> Result<(), IndexerError> {
    let files = match discover_terraform_files(root) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, root = %root.display(), "bulk scan: discovery failed");
            return Ok(());
        }
    };
    scan_files_parallel(state, client, files).await;
    Ok(())
}

/// Publish diagnostics for the document at `path` if it exists in
/// the store. Used after a background parse so workspace-wide views
/// (e.g. Trouble's `<leader>xx`) see diagnostics for indexed files
/// the user hasn't opened directly.
async fn publish_for_path(
    state: &StateStore,
    client: &tower_lsp::Client,
    path: &Path,
) {
    let Ok(uri) = tower_lsp::lsp_types::Url::from_file_path(path) else {
        return;
    };
    let version = match state.documents.get(&uri) {
        Some(doc) => doc.version,
        None => return,
    };
    let diagnostics = crate::handlers::document::compute_diagnostics(state, &uri);
    client
        .publish_diagnostics(uri, diagnostics, Some(version))
        .await;
}

/// Walk upward from `start` looking for a directory whose
/// `.terraform/providers/` subtree exists. That directory is the
/// terraform module root where `tofu init` was run and its schemas
/// live. Returns `None` if nothing was found before hitting the
/// filesystem root.
fn find_terraform_init_root(start: &Path) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(start);
    while let Some(dir) = current {
        if dir.join(".terraform").join("providers").is_dir() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

async fn scan_dir_into_state(
    state: &StateStore,
    queue: &JobQueue,
    client: Option<&tower_lsp::Client>,
    dir: &Path,
) -> Result<(), IndexerError> {
    match discover_terraform_files_in_dir(dir) {
        Ok(files) => {
            tracing::info!(count = files.len(), dir = %dir.display(), "module scan");
            scan_files_parallel(state, client, files).await;
            enqueue_child_module_scans(state, queue, dir);
        }
        Err(e) => tracing::warn!(error = %e, dir = %dir.display(), "module scan failed"),
    }
    Ok(())
}

/// Read + parse + upsert all `files` in parallel (rayon inside
/// `spawn_blocking`), then build a per-module-dir snapshot, compute
/// diagnostics in parallel, and fan out publishes concurrently.
/// Shared between [`scan_dir_into_state`] and
/// [`bulk_workspace_scan`] so both benefit from the same speedups.
async fn scan_files_parallel(
    state: &StateStore,
    client: Option<&tower_lsp::Client>,
    files: Vec<PathBuf>,
) {
    use rayon::prelude::*;

    let file_count = files.len();
    // Begin a progress token if we have a client attached. Silent
    // if the client declines the workDoneProgress/create request
    // (older clients).
    let progress = match client {
        Some(c) => crate::progress::ProgressReporter::begin(c, "Indexing Terraform workspace").await,
        None => None,
    };

    let parsed: Vec<DocumentState> = tokio::task::spawn_blocking({
        let files = files.clone();
        let skip: std::collections::HashSet<lsp_types::Url> = state
            .documents
            .iter()
            .map(|e| e.key().clone())
            .collect();
        move || {
            files
                .into_par_iter()
                .filter_map(|path| {
                    let url = path_to_url(&path)?;
                    if skip.contains(&url) {
                        return None;
                    }
                    let text = std::fs::read_to_string(&path).ok()?;
                    Some(DocumentState::new(url, &text, 0))
                })
                .collect()
        }
    })
    .await
    .unwrap_or_default();

    let parsed_count = parsed.len();
    if let Some(p) = progress.as_ref() {
        p.report(
            Some(format!("parsed {parsed_count}/{file_count}")),
            Some(33),
        )
        .await;
    }

    let mut uris: Vec<lsp_types::Url> = parsed.iter().map(|d| d.uri.clone()).collect();
    for doc in parsed {
        state.upsert_document(doc);
    }
    // Also include any already-open docs that sit in the same dirs
    // we just scanned — they should be in the publish round too so
    // cross-file aggregates (added-provider, etc.) refresh.
    for f in &files {
        if let Some(url) = path_to_url(f) {
            if !uris.contains(&url) && state.documents.contains_key(&url) {
                uris.push(url);
            }
        }
    }

    let Some(client) = client else {
        if let Some(p) = progress {
            p.end(None).await;
        }
        return;
    };

    if let Some(p) = progress.as_ref() {
        p.report(Some("computing diagnostics".to_string()), Some(66))
            .await;
    }

    // Group by parent dir so we build one ModuleSnapshot per module.
    let mut by_module: std::collections::HashMap<Option<PathBuf>, Vec<lsp_types::Url>> =
        std::collections::HashMap::new();
    for uri in uris {
        let dir = crate::handlers::util::parent_dir(&uri);
        by_module.entry(dir).or_default().push(uri);
    }

    let mut published = 0usize;
    for (dir, uris_in_module) in by_module {
        let snapshot = crate::handlers::module_snapshot::ModuleSnapshot::build(
            state,
            dir.as_deref(),
        );
        let snapshot_ref = &snapshot;
        let results: Vec<(lsp_types::Url, i32, Vec<lsp_types::Diagnostic>)> =
            tokio::task::block_in_place(|| {
                uris_in_module
                    .par_iter()
                    .filter_map(|uri| {
                        let version = state.documents.get(uri)?.version;
                        let lookup =
                            crate::handlers::module_snapshot::CachedModuleLookup {
                                snapshot: snapshot_ref,
                                state,
                                current_uri: uri,
                            };
                        let current_file = uri
                            .path_segments()
                            .and_then(|it| it.last())
                            .unwrap_or("")
                            .to_string();
                        let diagnostics =
                            crate::handlers::document::compute_diagnostics_with_lookup(
                                state,
                                uri,
                                &lookup,
                                &current_file,
                            );
                        Some((uri.clone(), version, diagnostics))
                    })
                    .collect()
            });

        published += results.len();
        use futures::stream::{FuturesUnordered, StreamExt};
        let mut pending: FuturesUnordered<_> = results
            .into_iter()
            .map(|(uri, version, diagnostics)| {
                let c = client.clone();
                async move {
                    c.publish_diagnostics(uri, diagnostics, Some(version)).await;
                }
            })
            .collect();
        while pending.next().await.is_some() {}
    }

    if let Some(p) = progress {
        p.end(Some(format!("indexed {published} files"))).await;
    }
}

/// After a directory's `.tf` files have been parsed into the store,
/// walk their `module_sources` and enqueue scans of any referenced
/// child module directories — whether local (relative paths) or
/// lockfile-resolved (remote modules cached under `.terraform/modules/`).
fn enqueue_child_module_scans(state: &StateStore, queue: &JobQueue, dir: &Path) {
    for entry in state.documents.iter() {
        let uri = entry.key();
        let Ok(doc_path) = uri.to_file_path() else {
            continue;
        };
        if doc_path.parent() != Some(dir) {
            continue;
        }
        for (label, source) in &entry.value().symbols.module_sources {
            let Some(child) = crate::handlers::util::resolve_module_source(dir, label, source)
            else {
                continue;
            };
            if state.scanned_dirs.contains(&child) {
                continue;
            }
            state.scanned_dirs.insert(child.clone());
            queue.enqueue(Job::ScanDirectory(child), Priority::Normal);
        }
    }
}

async fn parse_file_into_state(
    state: &StateStore,
    path: &Path,
) -> Result<(), IndexerError> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|source| IndexerError::FileRead {
            path: path.display().to_string(),
            source,
        })?;
    let Some(url) = path_to_url(path) else {
        tracing::warn!(path = %path.display(), "skipping: cannot form file URL");
        return Ok(());
    };

    // Don't overwrite an open document — the editor is authoritative
    // on in-memory state and the worker's disk snapshot may be stale.
    if state.documents.contains_key(&url) {
        return Ok(());
    }

    state.upsert_document(DocumentState::new(url, &text, 0));
    Ok(())
}

async fn fetch_and_install_schemas(
    state: &StateStore,
    working_dir: &Path,
) -> Result<(), IndexerError> {
    // Prefer the plugin-protocol path when `.terraform/providers/` exists:
    // it doesn't require credentials or backend init, and it reuses the
    // provider binaries terraform/tofu already downloaded.
    let terraform_dir = working_dir.join(".terraform");
    let providers_dir = terraform_dir.join("providers");
    if providers_dir.is_dir() {
        tracing::info!(
            dir = %terraform_dir.display(),
            "fetching provider schemas via plugin protocol",
        );
        match tfls_provider_protocol::fetch_schemas_from_plugins(&terraform_dir).await {
            Ok(schemas) if !schemas.provider_schemas.is_empty() => {
                let count = schemas.provider_schemas.len();
                state.install_schemas(schemas);
                tracing::info!(providers = count, "installed provider schemas (plugin)");

                // Also fetch provider-defined functions from v6 providers.
                if let Ok(binaries) = tfls_provider_protocol::discover_providers(&terraform_dir) {
                    for bin in &binaries {
                        match tfls_provider_protocol::client::fetch_provider_functions(bin).await {
                            Ok(funcs) if !funcs.is_empty() => {
                                let fcount = funcs.len();
                                state.merge_functions(funcs);
                                tracing::info!(
                                    count = fcount,
                                    provider = %bin.full_address(),
                                    "installed provider functions",
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::debug!(
                                    error = %e,
                                    provider = %bin.full_address(),
                                    "provider functions unavailable",
                                );
                            }
                        }
                    }
                }

                return Ok(());
            }
            Ok(_) => {
                tracing::warn!(
                    dir = %providers_dir.display(),
                    "plugin protocol returned no schemas; falling back to CLI",
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    dir = %providers_dir.display(),
                    "plugin protocol failed; falling back to CLI",
                );
            }
        }
    }

    tracing::info!(dir = %working_dir.display(), "fetching provider schemas via CLI");
    let fetcher = SchemaFetcher::new(working_dir.to_path_buf()).with_timeout(SCHEMA_FETCH_TIMEOUT);
    let schemas = fetcher.fetch().await.map_err(IndexerError::SchemaFetch)?;
    let count = schemas.provider_schemas.len();
    state.install_schemas(schemas);
    tracing::info!(providers = count, "installed provider schemas (CLI)");
    Ok(())
}

fn path_to_url(path: &Path) -> Option<lsp_types::Url> {
    lsp_types::Url::from_file_path(path).ok()
}
