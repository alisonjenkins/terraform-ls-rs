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
            if let Err(e) = handle_job(Arc::clone(&state), &queue, client.as_ref(), job).await {
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
pub fn enqueue_workspace_scan(_state: &StateStore, queue: &JobQueue, root: &Path) {
    // Discovery and pre-marking happen inside the
    // `BulkWorkspaceScan` job now, not here. Doing them inline in
    // `initialize` blocks the LSP initialize roundtrip on a
    // filesystem walk — seconds on large monorepos — which leaves
    // Fidget silent and makes the server feel hung. The job itself
    // opens a progress span before scanning so `$/progress`
    // notifications arrive as soon as the worker picks it up.
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
        maybe_enqueue_schema_fetch(state, queue, &init_root);
    }

    // Synchronous peer-index pull. Read + parse + upsert every
    // `.tf` in this directory BEFORE the caller's
    // `compute_diagnostics` runs. Without this, the first
    // diagnostic pass sees a half-populated store (only the
    // just-opened file is in it) and emits false-positive
    // "undefined variable" / "undefined module" diagnostics for
    // every peer-declared symbol. Latency: tens of ms for a
    // typical module; scales with file count in a single
    // directory (not the whole workspace), so the cost is
    // bounded by module size, not monorepo size.
    //
    // The trade-off: `did_open` is slower by the time it takes
    // to read + parse those files. Previous commits tried the
    // async-plus-`workspace/diagnostic/refresh` route to keep
    // `did_open` fast, but the refresh signal turned out to be
    // unreliable in the user's client — each "fix" surfaced a
    // new symptom rooted in the same timing race. Eating the
    // synchronous cost here eliminates the entire class of bug
    // at the cost of a one-time latency hit per buffer open.
    index_module_dir_sync(state, &dir_buf);

    // Enqueue a sibling-dir scan only if nobody has scheduled one
    // yet. The bulk workspace scan marks dirs as `Scheduled` as
    // part of its discovery phase, so if this `did_open` fires
    // after that, `mark_scan_scheduled` returns `false` and we
    // skip (the bulk scan will cover it).
    //
    // The sync pull above has already populated peer files, so
    // this is just for downstream work: discovering + scanning
    // child modules referenced via `module.X { source = "..." }`,
    // and populating the push namespace for
    // `:Trouble workspace_diagnostics` coverage of unopened
    // peers.
    if state.mark_scan_scheduled(dir_buf.clone()) {
        queue.enqueue(Job::ScanDirectory(dir_buf.clone()), Priority::High);
    }
    enqueue_child_module_scans(state, queue, &dir_buf);
}

/// Synchronously read + parse + upsert every `.tf` / `.tf.json`
/// file in `dir` that isn't already in the store. Idempotent:
/// files already indexed (via prior did_open, editor-driven
/// edit, or completed async scan) are skipped.
///
/// Called from [`ensure_module_indexed`] to guarantee peer
/// files are in the store before `compute_diagnostics` runs on
/// the just-opened buffer. Without this pre-population, the
/// first diagnostic pass falsely reports cross-file references
/// as undefined because their declaring files haven't been
/// parsed yet.
///
/// Does NOT mark `dir` as Completed — that's the async
/// `ScanDirectory` job's responsibility, which ALSO computes
/// diagnostics for every file in the dir and pushes them
/// (workspace-view coverage, `:Trouble workspace_diagnostics`).
/// The sync pull is scoped strictly to "symbols in the store so
/// diagnostics for the opened buffer see them"; it doesn't
/// cover the push-publish side.
pub fn index_module_dir_sync(state: &StateStore, dir: &Path) {
    let files = match discover_terraform_files_in_dir(dir) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                error = %e,
                dir = %dir.display(),
                "sync module index: discovery failed"
            );
            return;
        }
    };
    let mut indexed = 0usize;
    for path in files {
        let Some(url) = path_to_url(&path) else {
            continue;
        };
        if state.documents.contains_key(&url) {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "sync module index: file read failed"
                );
                continue;
            }
        };
        state.upsert_document(DocumentState::new(url, &text, 0));
        indexed += 1;
    }
    tracing::debug!(
        dir = %dir.display(),
        indexed,
        "sync module index: complete"
    );
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
    state: Arc<StateStore>,
    queue: &JobQueue,
    client: Option<&tower_lsp::Client>,
    job: Job,
) -> Result<(), IndexerError> {
    let job_kind = job_kind_str(&job);
    let started = std::time::Instant::now();
    let result = dispatch_job(state, queue, client, job).await;
    tracing::info!(
        job_kind,
        elapsed_ms = started.elapsed().as_millis() as u64,
        ok = result.is_ok(),
        "job done"
    );
    result
}

fn job_kind_str(job: &Job) -> &'static str {
    match job {
        Job::ParseFile(_) => "ParseFile",
        Job::ReparseDocument(_) => "ReparseDocument",
        Job::FetchSchemas { .. } => "FetchSchemas",
        Job::FetchFunctions { .. } => "FetchFunctions",
        Job::ScanDirectory(_) => "ScanDirectory",
        Job::BulkWorkspaceScan(_) => "BulkWorkspaceScan",
    }
}

async fn dispatch_job(
    state: Arc<StateStore>,
    queue: &JobQueue,
    client: Option<&tower_lsp::Client>,
    job: Job,
) -> Result<(), IndexerError> {
    match job {
        Job::ParseFile(path) => {
            let result = parse_file_into_state(&state, &path).await;
            if let Some(c) = client {
                publish_for_path(&state, c, &path).await;
            }
            result
        }
        Job::ReparseDocument(url) => {
            state.reparse_document(&url);
            if let Some(c) = client {
                if !state.should_skip_push_diagnostics(&url) {
                    let diagnostics = crate::handlers::document::compute_diagnostics(&state, &url);
                    let version = state.documents.get(&url).map(|d| d.version);
                    c.publish_diagnostics(url, diagnostics, version).await;
                }
            }
            Ok(())
        }
        Job::FetchSchemas { working_dir } => {
            let progress = match client {
                Some(c) => {
                    let rep = crate::progress::ProgressReporter::begin(
                        c,
                        "Fetching provider schemas",
                    )
                    .await;
                    // Clue the user in that other startup work is
                    // behind this job in the queue — otherwise the
                    // progress feels like "just one thing happening"
                    // when the workspace scan is waiting to run.
                    if let Some(r) = rep.as_ref() {
                        r.report(
                            Some("workspace indexing queued after schemas".to_string()),
                            None,
                        )
                        .await;
                    }
                    rep
                }
                None => None,
            };
            // Per-provider progress callback — forwards schema-fetch
            // ticks to the LSP progress widget so the user sees e.g.
            // "3/14 — cloudflare" instead of a frozen "Fetching".
            let on_provider_done: Option<tfls_provider_protocol::SchemaProgressCallback> =
                progress.as_ref().map(|p| {
                    let sender = p.sender();
                    let cb: tfls_provider_protocol::SchemaProgressCallback =
                        std::sync::Arc::new(move |addr: &str, done: usize, total: usize| {
                            let addr_short = addr.rsplit('/').next().unwrap_or(addr).to_string();
                            let msg = format!("{done}/{total} — {addr_short}");
                            let pct = (done * 100)
                                .checked_div(total)
                                .map(|p| p as u32);
                            sender.send_detached(Some(msg), pct);
                        });
                    cb
                });
            let result = fetch_and_install_schemas(
                Arc::clone(&state),
                &working_dir,
                on_provider_done,
            )
            .await;
            if let Some(p) = progress {
                let message = match &result {
                    Ok(_) => Some("schemas ready".to_string()),
                    Err(_) => Some("schema fetch failed".to_string()),
                };
                p.end(message).await;
            }
            // User-visible "completion is ready to use" signal —
            // logMessage is durable in :LspLog and non-intrusive.
            // The count of installed providers gives the user a
            // confidence signal ("14 providers loaded") vs. the
            // previous "completion just doesn't work" confusion.
            if let (Some(c), Ok(())) = (client, result.as_ref()) {
                let count = state.schemas.len();
                c.log_message(
                    lsp_types::MessageType::INFO,
                    format!(
                        "terraform-ls-rs: schemas ready — {count} providers installed. \
                         Completion + hover structure work now; attribute descriptions \
                         load in the background."
                    ),
                )
                .await;
            }
            // Provider schemas are the basis for attribute-level
            // validation diagnostics. Before they're installed, open
            // buffers either have no schema-based diagnostics at all
            // or have ones computed against a stale / missing schema
            // — refresh so they reflect the new install.
            if result.is_ok() {
                maybe_refresh_diagnostics(&state, client).await;
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
        Job::ScanDirectory(dir) => scan_dir_into_state(&state, queue, client, &dir).await,
        Job::BulkWorkspaceScan(root) => bulk_workspace_scan(&state, client, &root).await,
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
    // Discovery happens here (not in `initialize`) so the LSP
    // initialize roundtrip returns immediately and the
    // `spawn_blocking` inside `scan_files_parallel` can overlap
    // with any other background jobs. Emit a short progress span
    // around the filesystem walk — a bare `Job::BulkWorkspaceScan`
    // with no prior progress would leave Fidget silent for the
    // (potentially multi-second) duration of the walk on large
    // repos.
    // Try the persistent index cache first. If a cache exists
    // for this workspace AND its entries still match the files
    // on disk, the bulk scan below finds those files already in
    // `state.documents` and skips parsing them entirely. Pure
    // savings for workspace re-opens: a 500-file workspace
    // drops from seconds of parse work to a stat-every-file
    // pass.
    let hydrate_start = std::time::Instant::now();
    // Cache load = sync `fs::read` + `serde_json::from_slice` over a
    // potentially megabyte-scale blob, and `hydrate_into_store`
    // then stats every cached file to validate mtime. All pure
    // blocking work — off the reactor.
    let hydrated = crate::blocking::run(|| {
        if let Some(cache) = tfls_state::IndexCache::load(root) {
            let n = cache.hydrate_into_store(state);
            tracing::info!(
                root = %root.display(),
                entries = n,
                elapsed_ms = hydrate_start.elapsed().as_millis() as u64,
                "index cache: hydrated"
            );
            n
        } else {
            0
        }
    });

    let discover_progress = match client {
        Some(c) => crate::progress::ProgressReporter::begin(c, "Discovering Terraform files").await,
        None => None,
    };
    // Workspace-wide `WalkBuilder` traversal. On a cold page
    // cache this walks thousands of directory entries and can
    // take seconds. Off the reactor thread — without this, the
    // "Discovering Terraform files" progress span sits frozen
    // and concurrent hovers / completions stall until the walk
    // returns.
    let files = match crate::blocking::run(|| discover_terraform_files(root)) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, root = %root.display(), "bulk scan: discovery failed");
            if let Some(p) = discover_progress {
                p.end(Some("discovery failed".to_string())).await;
            }
            return Ok(());
        }
    };
    tracing::info!(
        count = files.len(),
        root = %root.display(),
        "workspace scan (bulk)"
    );

    // Collect the unique set of dirs containing at least one
    // discovered `.tf` file, then mark them `Scheduled`. Any
    // `did_open` that fires while the bulk scan is running will
    // see the `Scheduled` state and skip enqueueing its own
    // redundant `ScanDirectory`. We upgrade to `Completed` below
    // once `scan_files_parallel` has actually parsed everything.
    let mut dirs: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    for path in &files {
        if let Some(parent) = path.parent() {
            dirs.insert(parent.to_path_buf());
        }
    }
    for dir in &dirs {
        state.mark_scan_scheduled(dir.clone());
    }
    if let Some(p) = discover_progress {
        p.end(Some(format!("{} files", files.len()))).await;
    }

    // Hand off to the parallel scan — it opens its own "Indexing
    // Terraform workspace" progress span with per-phase detail.
    scan_files_parallel(state, client, files, /* with_progress */ true).await;

    // The bulk scan has parsed every discoverable `.tf` in every
    // dir. Upgrade each to `Completed` so correctness-sensitive
    // callers (e.g. diagnostic passes that depend on cross-file
    // symbols being present) can gate on it. While we're walking
    // the dirs, also recompute the assigned-variable-types map for
    // each — every tfvars file + every module-call attribute now
    // contributes to the map, which the type-inference code action
    // reads to suggest `type = …` for variables that have no
    // `default`.
    for dir in dirs {
        // TODO(diag-regression): bisect — disabled per plan
        // /home/ali/.claude/plans/is-it-possible-for-expressive-cook.md
        // Suspected cause of post-bulk diagnostics dropout.
        // rebuild_assigned_variable_types_for_dir(state, &dir);
        state.mark_scan_completed(dir);
    }

    // The bulk scan just filled the store with peer files open
    // buffers may depend on (cross-file symbols, modules declared in
    // siblings). Nudge the client to re-pull diagnostics so those
    // open buffers don't sit on false-positives from the earlier
    // `did_open` pass that only saw the buffer itself.
    maybe_refresh_diagnostics(state, client).await;

    // Save the post-scan store back to the persistent index
    // cache so the NEXT server start on this workspace can
    // skip the parse phase for unchanged files. This
    // deliberately happens AFTER the scan completes so the
    // cache reflects the freshly-parsed state, not whatever
    // was hydrated at the start (which may have been stale
    // for files the user edited outside the editor).
    let save_start = std::time::Instant::now();
    let cache = tfls_state::IndexCache::capture(state, root);
    let entry_count = cache.entries.len();
    let root_owned = root.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || cache.save(&root_owned)).await;
    tracing::info!(
        root = %root.display(),
        hydrated_on_load = hydrated,
        entries_saved = entry_count,
        elapsed_ms = save_start.elapsed().as_millis() as u64,
        "index cache: saved"
    );
    Ok(())
}

/// What the server should do after a background scan changes the
/// store — pure decision function so the invariants are unit-
/// testable without mocking the `tower_lsp::Client`.
///
/// Critical invariant: under pull-diagnostics mode, the server
/// MUST NOT `publishDiagnostics` for open buffers. Nvim (and
/// other clients that track push and pull in separate namespaces)
/// will display the pushed entry AND the subsequent pull entry as
/// two diagnostics with the same message — the exact "duplicate
/// diagnostic" symptom we've regressed into twice now. The only
/// workspace-wide notification mechanism compatible with
/// pull-mode is `workspace/diagnostic/refresh`, so we either send
/// that or do nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshDecision {
    /// No client attached — tests or headless runs.
    NoClient,
    /// Send `workspace/diagnostic/refresh`. Client re-pulls,
    /// replacing its single-namespace pull entry with fresh data.
    SendRefresh,
    /// Do nothing. Either the client is push-only (open buffers
    /// already got pushed by the regular scan flow) or it's
    /// pull-without-refresh (any push would duplicate against
    /// the pull namespace — user sees "variable has no type"
    /// listed twice). Pull-without-refresh clients keep a stale
    /// pre-scan pull result until their next edit-triggered
    /// pull; that's strictly better than visible duplicates.
    NoOp,
}

fn decide_refresh(
    state: &StateStore,
    client_attached: bool,
) -> RefreshDecision {
    if !client_attached {
        return RefreshDecision::NoClient;
    }
    if state.client_supports_diagnostic_refresh() {
        return RefreshDecision::SendRefresh;
    }
    RefreshDecision::NoOp
}

/// Notify the client that diagnostics for open buffers may have
/// changed because a background scan added peer files to the
/// store. Thin async wrapper around [`decide_refresh`] — the
/// decision logic lives there so tests can verify the
/// no-duplicate-push invariant without mocking the client.
pub(crate) async fn maybe_refresh_diagnostics(
    state: &StateStore,
    client: Option<&tower_lsp::Client>,
) {
    match decide_refresh(state, client.is_some()) {
        RefreshDecision::NoClient | RefreshDecision::NoOp => {}
        RefreshDecision::SendRefresh => {
            if let Some(c) = client {
                if let Err(e) = c.workspace_diagnostic_refresh().await {
                    tracing::warn!(error = ?e, "workspace/diagnostic/refresh failed");
                }
            }
        }
    }
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
    if state.should_skip_push_diagnostics(&uri) {
        return;
    }
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

/// Walk upward from `file_uri`'s directory looking for the
/// nearest `.terraform/providers/` root, and enqueue a schema
/// fetch when the providers directory's mtime differs from the
/// mtime recorded on the last fetch. Used by both `did_open`
/// (via [`ensure_module_indexed`]) and `did_save` to catch the
/// common sequence: user adds a new provider to `.tf` → runs
/// `tofu init` / `terraform init` in a shell → expects the
/// LSP to see the new provider. Without the mtime-gated
/// re-fetch, `fetched_schema_dirs` would permanently suppress
/// subsequent fetches for the same init root and the new
/// provider's schema would never load.
pub fn refresh_schemas_if_providers_changed(
    state: &StateStore,
    queue: &JobQueue,
    file_uri: &lsp_types::Url,
) {
    let Ok(path) = file_uri.to_file_path() else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    let Some(init_root) = find_terraform_init_root(dir) else {
        return;
    };
    maybe_enqueue_schema_fetch(state, queue, &init_root);
}

fn maybe_enqueue_schema_fetch(state: &StateStore, queue: &JobQueue, init_root: &Path) {
    // Use the mtime of `.terraform/providers/` as a change
    // signal. `tofu init` / `terraform init` install provider
    // binaries into subdirectories of this path; on every
    // standard filesystem, creating / updating a child bumps
    // the parent directory's mtime. Gate the enqueue on "this
    // mtime differs from the one we stored at the last
    // fetch". First sight (no entry) always fetches. If the
    // stat fails we fall back to the old one-shot behaviour
    // via the present-but-unchanged check.
    let providers_dir = init_root.join(".terraform").join("providers");
    let Some(current_mtime) = std::fs::metadata(&providers_dir)
        .ok()
        .and_then(|m| m.modified().ok())
    else {
        // Can't stat — treat as "nothing changed" so we don't
        // spam fetches on flaky filesystems.
        return;
    };
    let prev = state
        .fetched_schema_dirs
        .get(init_root)
        .map(|e| *e.value());
    if prev == Some(current_mtime) {
        return;
    }
    // Record the new mtime BEFORE enqueueing so a rapid second
    // did_open on the same root doesn't stack duplicate
    // fetches — the job runs asynchronously.
    state
        .fetched_schema_dirs
        .insert(init_root.to_path_buf(), current_mtime);
    queue.enqueue(
        Job::FetchSchemas {
            working_dir: init_root.to_path_buf(),
        },
        Priority::Normal,
    );
}

async fn scan_dir_into_state(
    state: &StateStore,
    queue: &JobQueue,
    client: Option<&tower_lsp::Client>,
    dir: &Path,
) -> Result<(), IndexerError> {
    // Single-directory `read_dir` — small but on the hot
    // `did_open` peer-indexing path, so wrap to keep the reactor
    // responsive when the user opens files on slow disks.
    match crate::blocking::run(|| discover_terraform_files_in_dir(dir)) {
        Ok(files) => {
            tracing::info!(count = files.len(), dir = %dir.display(), "module scan");
            // Per-directory scans are typically sub-ms and fire in
            // rapid bursts (one per child-module, plus each
            // did_open). Surfacing a fresh `Indexing Terraform
            // workspace` progress token for every one floods the
            // client's Fidget widget with begin/end churn: Nvim
            // observers see the label stuck at 99% of the last
            // one to arrive even though every token has been
            // properly ended on the server. Only the bulk scan
            // (which actually takes visible time) reports progress
            // now; per-dir scans index silently.
            scan_files_parallel(state, client, files, /* with_progress */ false).await;
            state.mark_scan_completed(dir.to_path_buf());
            // TODO(diag-regression): bisect — disabled per plan
            // /home/ali/.claude/plans/is-it-possible-for-expressive-cook.md
            // Suspected cause of per-dir scan diagnostics dropout.
            // rebuild_assigned_variable_types_for_dir(state, dir);
            enqueue_child_module_scans(state, queue, dir);
            // Cross-file symbols just changed — any open buffer in
            // this directory (or referencing this directory via a
            // module source) may have stale diagnostics. Refresh.
            maybe_refresh_diagnostics(state, client).await;
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
    with_progress: bool,
) {
    use rayon::prelude::*;

    let file_count = files.len();
    let total_start = std::time::Instant::now();
    // Begin a progress token only when the caller opts in. The
    // bulk workspace scan uses it for the long-running initial
    // indexing; per-directory scans stay silent to avoid flooding
    // the progress widget with rapid begin/end churn (dozens of
    // child-module scans complete in single-digit ms each).
    let progress = match (client, with_progress) {
        (Some(c), true) => {
            crate::progress::ProgressReporter::begin(c, "Indexing Terraform workspace").await
        }
        _ => None,
    };

    let parse_start = std::time::Instant::now();
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
    tracing::info!(
        parsed = parsed_count,
        total = file_count,
        elapsed_ms = parse_start.elapsed().as_millis() as u64,
        "scan_files_parallel: parse done"
    );
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
    let diag_start = std::time::Instant::now();

    // Group by parent dir so we build one ModuleSnapshot per module.
    let mut by_module: std::collections::HashMap<Option<PathBuf>, Vec<lsp_types::Url>> =
        std::collections::HashMap::new();
    for uri in uris {
        let dir = crate::handlers::util::parent_dir(&uri);
        by_module.entry(dir).or_default().push(uri);
    }

    // Intermediate progress — so the user sees movement while the
    // blocking `referenced_dirs_in_workspace` precompute runs. On a
    // cold filesystem cache the canonicalize syscalls below can
    // take seconds, and without this tick the 66% "computing
    // diagnostics" label would sit frozen until the first module
    // finishes publishing.
    if let Some(p) = progress.as_ref() {
        p.report(Some("building module graph".to_string()), Some(67))
            .await;
    }

    // Precompute the workspace-wide "referenced-by-some-module-call"
    // directory set ONCE. Every `ModuleSnapshot::build` call below
    // uses it for an O(1) `is_root` determination instead of the
    // O(N) per-snapshot walk + canonicalize round-trips that
    // previously dominated the compute phase (the 66%-frozen
    // Fidget progress the user reported). The walk does one
    // `canonicalize` syscall per local module source — potentially
    // hundreds on large stacks — so defer to a blocking thread to
    // keep the runtime reactor responsive.
    let state_for_precompute = state;
    let referenced_dirs = crate::blocking::run(|| {
        crate::handlers::module_snapshot::referenced_dirs_in_workspace(state_for_precompute)
    });

    let total_modules = by_module.len();
    let mut published = 0usize;
    for (idx, (dir, uris_in_module)) in by_module.into_iter().enumerate() {
        // Per-module progress report — emitted BEFORE the compute
        // step so the user sees "N/M" tick up as each module starts,
        // not when it finishes. This matters when an individual
        // module's publish drain stalls on a slow client: the old
        // after-drain placement could leave the label frozen for
        // seconds at a time while the user waits for the client to
        // read from the stdout pipe. Percent interpolates between
        // 66 (phase start) and 99 (leaving one point for `end`).
        if let Some(p) = progress.as_ref() {
            let started = idx + 1;
            let pct = (started * 33)
                .checked_div(total_modules)
                .map_or(99, |p| 66 + p as u32);
            p.report(
                Some(format!(
                    "computing diagnostics ({started}/{total_modules} modules)"
                )),
                Some(pct),
            )
            .await;
        }

        let snapshot = crate::handlers::module_snapshot::ModuleSnapshot::build(
            state,
            dir.as_deref(),
            Some(&referenced_dirs),
        );
        let snapshot_ref = &snapshot;
        let results: Vec<(lsp_types::Url, i32, Vec<lsp_types::Diagnostic>)> =
            crate::blocking::run(|| {
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
                            .and_then(|mut it| it.next_back())
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
            .filter(|(uri, _, _)| !state.should_skip_push_diagnostics(uri))
            .map(|(uri, version, diagnostics)| {
                let c = client.clone();
                async move {
                    c.publish_diagnostics(uri, diagnostics, Some(version)).await;
                }
            })
            .collect();
        while pending.next().await.is_some() {}
    }

    tracing::info!(
        published,
        elapsed_ms = diag_start.elapsed().as_millis() as u64,
        total_ms = total_start.elapsed().as_millis() as u64,
        "scan_files_parallel: diagnostics + publish done"
    );
    if let Some(p) = progress {
        p.end(Some(format!("indexed {published} files"))).await;
    }
}

/// Recompute `state.assigned_variable_types` for `dir` and for every
/// child module dir that any `.tf` in `dir` references via a
/// `module "X" { source = "./Y" … }` block. Two sources contribute:
///
/// 1. **Tfvars in `dir`** (`*.tfvars`, `*.auto.tfvars`,
///    `*.tfvars.json`). Each top-level `name = value` assignment
///    becomes an entry under `state.assigned_variable_types[dir][name]`
///    with the inferred shape.
/// 2. **Module-call attributes from `.tf` files in `dir`**. For each
///    `module "X" { src = "./Y", attr = expr }`, resolve `Y` to the
///    child directory and add `attr → infer(expr)` under
///    `state.assigned_variable_types[child_dir][attr]`. Multiple
///    callers / multiple env-specific tfvars accumulate into the
///    inner `Vec`; the consumer (the type-inference code action)
///    equality-merges across them.
///
/// Wholesale replacement: every call rebuilds the entries for all
/// affected target dirs from a current snapshot, so a removed
/// caller or deleted tfvars file doesn't leave a stale type
/// hanging around.
// Currently disabled — see TODO(diag-regression) at call sites.
#[allow(dead_code)]
fn rebuild_assigned_variable_types_for_dir(state: &StateStore, dir: &Path) {
    use std::collections::HashMap;
    use tfls_core::variable_type::{VariableType, parse_value_shape_with_schema};

    // Skip meta-attributes that aren't user-declared module inputs.
    fn is_meta_attr(name: &str) -> bool {
        matches!(
            name,
            "source" | "version" | "providers" | "count" | "for_each" | "depends_on"
        )
    }

    // Collect target_dir → (var_name → list of types) so we can
    // replace each affected dir's entry atomically at the end.
    let mut staged: HashMap<PathBuf, HashMap<String, Vec<VariableType>>> = HashMap::new();

    // 1. Tfvars in `dir` → assignments target `dir` itself.
    if let Ok(tfvars) = tfls_walker::discover_tfvars_files_in_dir(dir) {
        let mut for_dir: HashMap<String, Vec<VariableType>> = HashMap::new();
        for path in tfvars {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (name, ty) in tfls_parser::parse_tfvars(&text) {
                for_dir.entry(name).or_default().push(ty);
            }
        }
        if !for_dir.is_empty() {
            staged.insert(dir.to_path_buf(), for_dir);
        }
    }

    // 2. Module calls authored in `.tf` files in `dir`. Each
    //    contributes assignments to its CHILD module's directory.
    for entry in state.documents.iter() {
        let Ok(doc_path) = entry.key().to_file_path() else {
            continue;
        };
        if doc_path.parent() != Some(dir) {
            continue;
        }
        let Some(body) = entry.value().parsed.body.as_ref() else {
            continue;
        };
        for structure in body.iter() {
            let Some(block) = structure.as_block() else {
                continue;
            };
            if block.ident.as_str() != "module" {
                continue;
            }
            let Some(label) = block.labels.first().map(|l| match l {
                hcl_edit::structure::BlockLabel::String(s) => s.value().to_string(),
                hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
            }) else {
                continue;
            };
            let Some(source) = entry.value().symbols.module_sources.get(&label).cloned()
            else {
                continue;
            };
            let Some(child_dir) =
                crate::handlers::util::resolve_module_source(dir, &label, &source)
            else {
                continue;
            };
            // Walk each attribute of the module block; infer the
            // type of its RHS; stage under the child dir.
            let bucket = staged.entry(child_dir).or_default();
            for body_struct in block.body.iter() {
                let Some(attr) = body_struct.as_attribute() else {
                    continue;
                };
                let attr_name = attr.key.as_str();
                if is_meta_attr(attr_name) {
                    continue;
                }
                let ty = parse_value_shape_with_schema(&attr.value, state);
                if matches!(&ty, VariableType::Any) {
                    continue;
                }
                if let VariableType::Tuple(items) = &ty {
                    if items.is_empty() {
                        continue;
                    }
                }
                if let VariableType::Object(fields) = &ty {
                    if fields.is_empty() {
                        continue;
                    }
                }
                bucket.entry(attr_name.to_string()).or_default().push(ty);
            }
        }
    }

    for (target_dir, assignments) in staged {
        state.replace_assigned_variable_types(target_dir, assignments);
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
            // `mark_scan_scheduled` returns `false` if the child dir
            // is already Scheduled or Completed — either way we
            // shouldn't enqueue again. Background priority: child
            // module scanning isn't tied to a specific user
            // cursor, so Normal (not High) is the right tier.
            if state.mark_scan_scheduled(child.clone()) {
                queue.enqueue(Job::ScanDirectory(child), Priority::Normal);
            }
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
    state: Arc<StateStore>,
    working_dir: &Path,
    on_provider_done: Option<tfls_provider_protocol::SchemaProgressCallback>,
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
        match tfls_provider_protocol::fetch_schemas_from_plugins_raw(
            &terraform_dir,
            on_provider_done,
        ).await {
            Ok(raw) if !raw.schemas.provider_schemas.is_empty() => {
                let count = raw.schemas.provider_schemas.len();
                // Install the bare schemas IMMEDIATELY so completion /
                // hover see them. Attribute descriptions arrive
                // asynchronously via the enrichment task we spawn
                // below — without this split, users wait the full
                // registry-doc round-trip (~60 s for aws) before
                // completion unblocks.
                state.install_schemas(raw.schemas.clone());
                tracing::info!(
                    providers = count,
                    "installed provider schemas (plugin, pre-enrichment)"
                );

                // Enrichment in the background. Clones the current
                // installed ProviderSchemas, mutates it, and
                // re-installs once done so hover descriptions light
                // up without blocking completion.
                spawn_background_enrichment(
                    Arc::clone(&state),
                    raw.schemas,
                    raw.coords,
                );

                // Also fetch provider-defined functions from v6 providers.
                // Dedupe by (ns, name) and reuse a single mTLS identity —
                // same reasons as the schema fetch above.
                if let Ok(binaries) = tfls_provider_protocol::discover_providers(&terraform_dir) {
                    let binaries =
                        tfls_provider_protocol::dedupe_providers_keep_highest(binaries);
                    let func_identity = match tfls_provider_protocol::tls::ClientIdentity::generate() {
                        Ok(id) => Some(std::sync::Arc::new(id)),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "could not generate shared mTLS identity; each function fetch will regenerate"
                            );
                            None
                        }
                    };
                    for bin in &binaries {
                        match tfls_provider_protocol::client::fetch_provider_functions(
                            bin,
                            func_identity.as_deref(),
                        ).await {
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

/// Run registry-doc enrichment off the critical path. Completion and
/// hover already work against the bare schemas we installed
/// synchronously; this fills in attribute descriptions (hover docs)
/// as the registry round-trips complete. Re-installs the enriched
/// schemas at the end, which overwrites the existing
/// `Arc<ProviderSchema>` entries atomically in the DashMap — any
/// concurrent reader sees either the old or the new, never a torn
/// view.
fn spawn_background_enrichment(
    state: Arc<StateStore>,
    mut schemas: tfls_schema::ProviderSchemas,
    coords: Vec<tfls_provider_protocol::registry_docs::ProviderCoords>,
) {
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        match tfls_provider_protocol::registry_docs::enrich_schemas_with_registry_docs(
            &mut schemas,
            &coords,
        )
        .await
        {
            Ok(updated) => {
                let elapsed_ms = start.elapsed().as_millis() as u64;
                if updated == 0 {
                    tracing::info!(
                        elapsed_ms,
                        "registry enrichment produced no updates; no re-install"
                    );
                    return;
                }
                state.install_schemas(schemas);
                tracing::info!(
                    attributes_updated = updated,
                    elapsed_ms,
                    "registry enrichment complete; re-installed enriched schemas",
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "registry enrichment failed — bare schemas remain installed",
                );
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! Invariant tests for the background scan's
    //! "notify-diagnostics-changed" path.
    //!
    //! **The invariant:** under pull-diagnostics mode the server
    //! must never `publishDiagnostics` for an open buffer. Nvim
    //! (and any client that stores push and pull diagnostics in
    //! separate namespaces) shows the pushed entry AND the
    //! subsequent pull entry as two duplicate diagnostics on the
    //! same line — the "duplicate diagnostic" bug we've regressed
    //! into twice. These tests pin the decision logic so a future
    //! commit can't silently reintroduce a push branch into
    //! `maybe_refresh_diagnostics`.
    //!
    //! The pure [`decide_refresh`] function makes the contract
    //! testable without mocking the `tower_lsp::Client`: its
    //! output enumerates exactly what the async wrapper will do,
    //! and `SendRefresh` is the ONLY variant that can trigger
    //! client-side I/O.
    use super::{RefreshDecision, decide_refresh, index_module_dir_sync};
    use std::fs;
    use std::path::PathBuf;
    use tfls_state::StateStore;
    use tower_lsp::lsp_types::Url;

    fn u(s: &str) -> Url {
        Url::parse(s).expect("valid url")
    }

    fn tmp_dir(label: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "tfls-indexer-{label}-{}-{nanos}",
            std::process::id(),
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create tmpdir");
        dir
    }

    #[test]
    fn decide_refresh_no_client_is_a_noop() {
        let store = StateStore::new();
        assert_eq!(
            decide_refresh(&store, /*client_attached=*/ false),
            RefreshDecision::NoClient
        );
    }

    #[test]
    fn decide_refresh_sends_when_client_supports_refresh() {
        let store = StateStore::new();
        store.set_client_supports_diagnostic_refresh(true);
        assert_eq!(
            decide_refresh(&store, true),
            RefreshDecision::SendRefresh
        );
    }

    #[test]
    fn decide_refresh_no_push_for_pull_without_refresh() {
        // CRITICAL regression test: a client that advertises pull
        // diagnostics but NOT `refresh_support` must get a `NoOp`.
        // Pushing here would duplicate against the pull namespace,
        // which is the exact "variable has no type" / "variable
        // has no type" duplicate users keep hitting.
        let store = StateStore::new();
        store.set_client_supports_pull_diagnostics(true);
        store.set_client_supports_diagnostic_refresh(false);
        store.mark_open(u("file:///stack/main.tf"));
        assert_eq!(
            decide_refresh(&store, true),
            RefreshDecision::NoOp,
            "pull-without-refresh MUST be a no-op; \
             pushing here duplicates against pull namespace"
        );
    }

    #[test]
    fn decide_refresh_no_push_for_push_only_client() {
        // Push-only clients (neither pull nor refresh advertised)
        // already got their open buffers pushed by the regular
        // scan-publish loop in `scan_files_parallel`. No further
        // action needed here.
        let store = StateStore::new();
        // Both flags default to false.
        assert_eq!(decide_refresh(&store, true), RefreshDecision::NoOp);
    }

    #[test]
    fn decide_refresh_refresh_wins_over_pull() {
        // When a client advertises BOTH pull and refresh, refresh
        // is authoritative. We never fall through to any push
        // path, regardless of how many open buffers are in the
        // state.
        let store = StateStore::new();
        store.set_client_supports_pull_diagnostics(true);
        store.set_client_supports_diagnostic_refresh(true);
        store.mark_open(u("file:///stack/main.tf"));
        store.mark_open(u("file:///stack/other.tf"));
        assert_eq!(
            decide_refresh(&store, true),
            RefreshDecision::SendRefresh
        );
    }

    #[test]
    fn schema_refetch_fires_when_providers_mtime_changes() {
        // User scenario: LSP server is already running with
        // some providers loaded. User adds a new provider to
        // their `.tf` and runs `tofu init`, which creates a
        // new subdirectory under `.terraform/providers/` —
        // bumping the parent directory's mtime. The next
        // did_open / did_save MUST re-enqueue a FetchSchemas
        // job so the new provider's schema loads. Without
        // this, `state.schemas` stays permanently stuck on
        // the pre-`tofu init` set.
        use super::{find_terraform_init_root, maybe_enqueue_schema_fetch};
        use std::fs;
        use tfls_state::{JobQueue, StateStore};

        let tree = tmp_dir("schema-mtime-refetch");
        fs::create_dir_all(tree.join(".terraform").join("providers")).unwrap();
        let init_root = find_terraform_init_root(&tree).expect("init root present");

        let state = StateStore::new();
        let queue = JobQueue::new();

        // First check: no prior entry → enqueue.
        maybe_enqueue_schema_fetch(&state, &queue, &init_root);
        assert_eq!(
            queue.len(),
            1,
            "first sight must enqueue a FetchSchemas job"
        );
        // Drain so the next check starts clean.
        while queue.try_next().is_some() {}

        // Second check without any filesystem change: the
        // stored mtime equals the current mtime → skip.
        maybe_enqueue_schema_fetch(&state, &queue, &init_root);
        assert_eq!(
            queue.len(),
            0,
            "no-change must NOT re-enqueue"
        );

        // Simulate `tofu init` installing a new provider by
        // creating a subdirectory under `.terraform/providers/`.
        // That bumps the parent dir's mtime.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::create_dir_all(
            init_root
                .join(".terraform")
                .join("providers")
                .join("registry.terraform.io")
                .join("hashicorp")
                .join("http"),
        )
        .unwrap();

        // Third check: mtime differs → enqueue.
        maybe_enqueue_schema_fetch(&state, &queue, &init_root);
        assert_eq!(
            queue.len(),
            1,
            "providers-dir mtime bump must re-enqueue"
        );

        let _ = fs::remove_dir_all(&tree);
    }

    #[test]
    fn refresh_decision_has_no_push_variant() {
        // Meta-invariant: the `RefreshDecision` enum must not
        // gain a variant that publishes diagnostics. If someone
        // adds e.g. `PushToOpenBuffers`, this test needs updating
        // — and the code reviewer must consciously accept the
        // duplicate-diagnostic risk before doing so. Match
        // exhaustively so adding a variant forces a decision.
        let variants = [
            RefreshDecision::NoClient,
            RefreshDecision::SendRefresh,
            RefreshDecision::NoOp,
        ];
        for v in variants {
            match v {
                RefreshDecision::NoClient
                | RefreshDecision::SendRefresh
                | RefreshDecision::NoOp => {}
            }
        }
    }

    // --- `index_module_dir_sync` ------------------------------------

    #[test]
    fn index_module_dir_sync_reads_parses_upserts() {
        let dir = tmp_dir("sync-reads-parses-upserts");
        fs::write(
            dir.join("variables.tf"),
            "variable \"region\" {}\n",
        )
        .unwrap();
        fs::write(
            dir.join("outputs.tf"),
            "output \"x\" { value = var.region }\n",
        )
        .unwrap();

        let store = StateStore::new();
        index_module_dir_sync(&store, &dir);

        let vars_uri = Url::from_file_path(dir.join("variables.tf")).unwrap();
        let out_uri = Url::from_file_path(dir.join("outputs.tf")).unwrap();
        assert!(
            store.documents.contains_key(&vars_uri),
            "variables.tf must be upserted"
        );
        assert!(
            store.documents.contains_key(&out_uri),
            "outputs.tf must be upserted"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_module_dir_sync_skips_already_indexed() {
        let dir = tmp_dir("sync-skips-already-indexed");
        let path = dir.join("variables.tf");
        fs::write(&path, "variable \"region\" {}\n").unwrap();

        let store = StateStore::new();
        let uri = Url::from_file_path(&path).unwrap();
        // Pre-populate with a SPECIFIC version so we can tell if
        // the sync helper overwrote it.
        store.upsert_document(tfls_state::DocumentState::new(
            uri.clone(),
            "variable \"DIFFERENT\" {}\n",
            42,
        ));

        index_module_dir_sync(&store, &dir);

        let doc = store.documents.get(&uri).expect("still there");
        assert_eq!(
            doc.version, 42,
            "sync index must skip already-indexed files — found overwrite"
        );
        assert!(
            doc.symbols.variables.contains_key("DIFFERENT"),
            "sync index overwrote an already-indexed document"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_module_dir_sync_tolerates_missing_dir() {
        // Nonexistent directory must NOT panic. `discover_*`
        // returns an error that we log and continue past.
        let store = StateStore::new();
        let dir = std::env::temp_dir().join("tfls-indexer-does-not-exist");
        let _ = fs::remove_dir_all(&dir);
        index_module_dir_sync(&store, &dir);
        // Store should be empty and no panic.
        assert_eq!(store.documents.len(), 0);
    }

    #[test]
    fn index_module_dir_sync_does_not_mark_completed() {
        // The async `ScanDirectory` job is responsible for
        // marking Completed (because it ALSO runs the diagnostic
        // compute + publish loop that completes the state
        // transition's contract). The sync pull only pre-
        // populates the store; it must NOT claim the dir is
        // Completed because the diagnostic-publish side hasn't
        // run yet.
        let dir = tmp_dir("sync-does-not-mark-completed");
        fs::write(dir.join("a.tf"), "variable \"x\" {}\n").unwrap();
        let store = StateStore::new();
        index_module_dir_sync(&store, &dir);
        assert!(
            !store.is_scan_completed(&dir),
            "sync index must NOT mark Completed — that's the async job's job"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
