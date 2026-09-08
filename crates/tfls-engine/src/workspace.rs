//! One-call workspace loader and a parallel whole-workspace lint.
//!
//! Shared entry point for anything that wants "index this directory,
//! then lint every file in it" without hand-rolling the discover →
//! parse/upsert → schema-fetch → per-dir-rebuild sequence: today
//! `tfls-diag-dump`, tomorrow the standalone `tfls-lint` CLI.
//!
//! Runtime-agnostic: [`load`] is a plain `async fn` with no `tokio`
//! dependency in this crate. Callers on a tokio runtime drive it from
//! their own `block_on` / `.await`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use tfls_state::StateStore;
use url::Url;

use crate::index::{self, ParseAndUpsert};

/// Where [`load`] should get provider schemas from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchemaSource {
    /// Walk up from the workspace root for `.terraform/providers/` and
    /// speak the plugin protocol to whatever provider binaries are
    /// installed there. The primary, most complete path — matches
    /// what `terraform init` + a live `did_open` would see.
    #[default]
    Plugins,
    /// Skip the plugin walk entirely and install only the compiled-in
    /// `terraform` provider schema (`terraform_remote_state`,
    /// `terraform_data`). Cheap and fully offline, but every other
    /// provider's schema-driven diagnostics (unknown attribute,
    /// schema-declared deprecations, missing tags, ...) stay silent.
    Bundled,
    /// Install no provider schema at all — not even the bundled
    /// `terraform` provider. Fastest option; widest set of silent
    /// diagnostic families.
    None,
}

/// Options for [`load`].
#[derive(Debug, Clone, Copy, Default)]
pub struct LoadOptions {
    pub schemas: SchemaSource,
}

/// What happened when [`load`] tried to satisfy `LoadOptions::schemas`.
/// Callers print this to tell a user why schema-driven diagnostics are
/// (or aren't) firing, without re-deriving the reason from `Loaded`.
#[derive(Debug, Clone)]
pub enum SchemaOutcome {
    /// `SchemaSource::Plugins` found a `.terraform/providers/` root and
    /// installed `count` provider schemas from it.
    Fetched { init_root: PathBuf, count: usize },
    /// `SchemaSource::Plugins` found a `.terraform/providers/` root but
    /// the plugin-protocol fetch failed. Not a [`LoadError`] — matches
    /// `tfls-diag-dump`'s long-standing behaviour of degrading to
    /// schema-free diagnostics rather than aborting the whole run.
    FetchFailed { init_root: PathBuf, message: String },
    /// `SchemaSource::Plugins` walked up from the root and found no
    /// `.terraform/providers/` directory anywhere.
    NoInitRoot,
    /// `SchemaSource::Bundled` installed the compiled-in `terraform`
    /// provider schema only.
    Bundled,
    /// `SchemaSource::None` — schema fetch was skipped entirely.
    Skipped,
}

/// The result of [`load`]: a populated [`StateStore`] plus enough
/// bookkeeping for a caller to report what it loaded.
pub struct Loaded {
    pub state: StateStore,
    /// Number of `.tf` / `.tf.json` files discovered under the root.
    pub file_count: usize,
    /// Every distinct directory that contributed at least one indexed
    /// document, sorted.
    pub dirs: BTreeSet<PathBuf>,
    pub schema_outcome: SchemaOutcome,
}

/// One failure site per variant so a caller — or a stack trace reader
/// months later — can tell which step failed from the variant alone.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to canonicalise workspace root '{path}'")]
    Root {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to discover terraform files under '{root}'")]
    Discovery {
        root: PathBuf,
        #[source]
        source: tfls_walker::WalkerError,
    },
    #[error("failed to load the bundled functions schema")]
    Functions {
        #[source]
        source: tfls_schema::SchemaError,
    },
}

/// Discover, parse, index and (optionally) schema-fetch every
/// `.tf` / `.tf.json` file under `root` into a fresh [`StateStore`],
/// then run the same per-directory rebuild passes the LSP indexer runs
/// after a bulk scan (`assigned_variable_types`, caller-passed unknown
/// module vars) so `lint_all` sees the same cross-module context a live
/// `did_open` would.
///
/// A schema-fetch failure is recorded in [`Loaded::schema_outcome`]
/// rather than returned as an error — schema-driven diagnostics simply
/// go silent, matching the diagnostics pipeline's own graceful
/// degradation when schemas aren't available.
pub async fn load(root: &Path, opts: &LoadOptions) -> Result<Loaded, LoadError> {
    let root = root.canonicalize().map_err(|source| LoadError::Root {
        path: root.to_path_buf(),
        source,
    })?;

    let files =
        tfls_walker::discover_terraform_files(&root).map_err(|source| LoadError::Discovery {
            root: root.clone(),
            source,
        })?;
    let file_count = files.len();

    let state = StateStore::new();
    let ParseAndUpsert { .. } = index::parse_and_upsert_files(&state, &files);

    let functions = tfls_schema::functions_cache::bundled()
        .map_err(|source| LoadError::Functions { source })?;
    state.install_functions(functions);

    let schema_outcome = match opts.schemas {
        SchemaSource::None => SchemaOutcome::Skipped,
        SchemaSource::Bundled => {
            index::install_builtin_provider_schema(&state);
            SchemaOutcome::Bundled
        }
        SchemaSource::Plugins => match index::find_terraform_init_root(&root) {
            Some(init_root) => {
                let tf_dir = init_root.join(".terraform");
                match tfls_provider_protocol::fetch_schemas_from_plugins(&tf_dir, None).await {
                    Ok(schemas) => {
                        let count = schemas.provider_schemas.len();
                        state.install_schemas(schemas);
                        SchemaOutcome::Fetched { init_root, count }
                    }
                    Err(e) => SchemaOutcome::FetchFailed {
                        init_root,
                        message: e.to_string(),
                    },
                }
            }
            None => SchemaOutcome::NoInitRoot,
        },
    };

    let dirs: BTreeSet<PathBuf> = state
        .documents
        .iter()
        .filter_map(|entry| {
            entry
                .key()
                .to_file_path()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
        })
        .collect();
    for d in &dirs {
        state.mark_scan_completed(d.clone());
    }
    for d in &dirs {
        index::rebuild_assigned_variable_types_for_dir(&state, d);
        index::rebuild_unknown_module_vars_for_dir(&state, d);
    }

    Ok(Loaded {
        state,
        file_count,
        dirs,
        schema_outcome,
    })
}

/// Lint every indexed `.tf` / `.tf.json` / `.tftest.hcl` document in
/// `state`, grouped by parent directory so each module pays the
/// cross-file aggregation cost ([`crate::snapshot::ModuleSnapshot`])
/// once rather than once per file — the same O(N²) → O(N) trick the
/// LSP indexer's bulk-scan path uses. Diagnostic compute itself is
/// parallelised across documents with rayon.
///
/// Returns one entry per document, sorted by URL; each document's
/// diagnostics are sorted by `(line, character, code)` so output is
/// deterministic across runs and thread-scheduling orders.
pub fn lint_all(state: &StateStore) -> Vec<(Url, Vec<lsp_types::Diagnostic>)> {
    let mut by_module: std::collections::HashMap<Option<PathBuf>, Vec<Url>> =
        std::collections::HashMap::new();
    for entry in state.documents.iter() {
        let uri = entry.key();
        if !is_lintable(uri) {
            continue;
        }
        let dir = crate::module::parent_dir(uri);
        by_module.entry(dir).or_default().push(uri.clone());
    }

    let referenced_dirs = crate::snapshot::referenced_dirs_in_workspace(state);

    let mut results: Vec<(Url, Vec<lsp_types::Diagnostic>)> = Vec::new();
    for (module_dir, uris) in by_module {
        let snapshot = crate::snapshot::ModuleSnapshot::build(
            state,
            module_dir.as_deref(),
            Some(&referenced_dirs),
        );
        let module_results: Vec<(Url, Vec<lsp_types::Diagnostic>)> = uris
            .par_iter()
            .filter_map(|uri| {
                let lookup = crate::snapshot::CachedModuleLookup {
                    snapshot: &snapshot,
                    state,
                    current_uri: uri,
                };
                let current_file = uri
                    .path_segments()
                    .and_then(|mut it| it.next_back())
                    .unwrap_or("")
                    .to_string();
                let diagnostics = index::catch_file_diag(uri, || {
                    crate::pipeline::compute_diagnostics_with_lookup(
                        state,
                        uri,
                        &lookup,
                        &current_file,
                    )
                })?;
                Some((uri.clone(), diagnostics))
            })
            .collect();
        results.extend(module_results);
    }

    results.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
    for (_, diags) in &mut results {
        diags.sort_by(|a, b| {
            let key = |d: &lsp_types::Diagnostic| {
                let code = match &d.code {
                    Some(lsp_types::NumberOrString::String(s)) => s.clone(),
                    Some(lsp_types::NumberOrString::Number(n)) => n.to_string(),
                    None => String::new(),
                };
                (d.range.start.line, d.range.start.character, code)
            };
            key(a).cmp(&key(b))
        });
    }
    results
}

/// True for anything the diagnostics pipeline knows how to lint:
/// `.tf`, `.tf.json`, and `.tftest.hcl` / `.tftest.json` (the pipeline
/// gates test-file rules internally). Skips anything under a
/// `.terraform/` path segment — vendored module copies and provider
/// caches, never source of truth for a workspace's own diagnostics.
fn is_lintable(uri: &Url) -> bool {
    let Ok(path) = uri.to_file_path() else {
        return false;
    };
    if path.components().any(|c| c.as_os_str() == ".terraform") {
        return false;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    file_name.ends_with(".tf")
        || file_name.ends_with(".tf.json")
        || file_name.ends_with(".tftest.hcl")
        || file_name.ends_with(".tftest.json")
}
