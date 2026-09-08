//! Synchronous indexing bookkeeping shared by the LSP server's
//! background indexer and the (future) standalone lint CLI: sizing
//! the rayon pool used for CPU-bound indexing work, and populating a
//! [`StateStore`] from files on disk. The async job queue, file
//! watcher, and diagnostic-publish plumbing stay in `tfls-lsp` — this
//! module is transport-free.
//!
//! `index_module_dir_sync` and `rebuild_assigned_variable_types_for_dir`
//! stay in `tfls-lsp` rather than moving here: both call into
//! `tfls-walker`'s directory-discovery helpers, and `tfls-walker`
//! carries a hard `notify`/`tokio` dependency for its file watcher.
//! Pulling either function into this crate would drag `notify` into
//! `tfls-engine`'s dependency tree, defeating the point of a
//! transport-free engine a lint CLI can embed without an async
//! runtime or a filesystem watcher.

use std::path::{Path, PathBuf};

use tfls_state::{DocumentState, StateStore};

fn path_to_url(path: &Path) -> Option<url::Url> {
    url::Url::from_file_path(path).ok()
}

/// Size rayon's global thread pool so background parallel
/// work (the bulk workspace scan's parse + diagnostic-compute
/// passes) leaves headroom for the tokio runtime's LSP
/// handlers. Without this, `rayon::par_iter` saturates all
/// CPU cores during indexing and the async handlers (did_open,
/// did_change, hover, completion, pull diagnostics) queue up
/// waiting for CPU — the "LSP feels slow during indexing"
/// symptom.
///
/// Policy: reserve 2 cores for tokio workers, give everything
/// else to rayon. On a 2-core machine we clamp to 2 rayon
/// threads (floor of 1 is useless; 2 lets rayon still
/// parallelise).
///
/// Respects `TFLS_RAYON_THREADS` env override for users who
/// want to tune explicitly. Idempotent-ish: calling twice is
/// a hard error from rayon — it logs a warning and continues.
///
/// Call once at server startup, BEFORE the tokio runtime
/// dispatches any request that uses rayon.
pub fn configure_rayon_pool() {
    let total = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let from_env = std::env::var("TFLS_RAYON_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok());
    let target = match from_env {
        Some(n) if n >= 1 => n,
        _ => {
            // Leave 2 cores for tokio; floor at 2 so single-
            // and dual-core machines aren't starved of rayon
            // parallelism entirely.
            std::cmp::max(2, total.saturating_sub(2))
        }
    };
    match rayon::ThreadPoolBuilder::new()
        .num_threads(target)
        .thread_name(|i| format!("tfls-rayon-{i}"))
        .build_global()
    {
        Ok(()) => {
            tracing::info!(
                rayon_threads = target,
                total_cpus = total,
                "rayon pool sized for indexing headroom"
            );
        }
        Err(e) => {
            // Rayon only lets you set the global pool once.
            // Subsequent calls return an error; that's not
            // fatal — the pool already exists.
            tracing::warn!(
                error = %e,
                "rayon global pool already configured; leaving as-is"
            );
        }
    }
}

/// Install the bundled built-in `terraform` provider schema
/// (`terraform_remote_state`, `terraform_data`) into the global schema
/// store. This provider is compiled into Terraform core and never
/// arrives via the plugin protocol or `providers schema -json`, so we
/// inject the compiled-in snapshot once at session start. Infallible —
/// a decode failure is logged and leaves the store unchanged.
pub fn install_builtin_provider_schema(state: &StateStore) {
    match tfls_schema::bundled_builtin_provider() {
        Ok(schemas) => {
            state.install_schemas(schemas);
            tracing::debug!("installed bundled built-in terraform provider schema");
        }
        Err(e) => {
            tracing::error!(error = %e, "bundled built-in provider schema failed to load");
        }
    }
}

/// Walk upward from `start` looking for a directory whose
/// `.terraform/providers/` subtree exists. That directory is the
/// terraform module root where `tofu init` was run and its schemas
/// live. Returns `None` if nothing was found before hitting the
/// filesystem root.
pub fn find_terraform_init_root(start: &Path) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(start);
    while let Some(dir) = current {
        if dir.join(".terraform").join("providers").is_dir() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Result of [`parse_and_upsert_files`]: the URIs touched by the
/// parse pass (freshly parsed, plus any already-open doc that also
/// appears in the input file list) and how many files were freshly
/// parsed off disk (as opposed to skipped because an open buffer or
/// an already-parsed closed doc already covers them).
pub struct ParseAndUpsert {
    pub uris: Vec<url::Url>,
    pub parsed_count: usize,
}

/// Read + parse (rayon-parallel) + upsert every file in `files` into
/// `state`. Pure and synchronous (CPU-bound) — no diagnostic compute,
/// no publish, no LSP client. Callers on an async runtime should run
/// it off the reactor.
///
/// Skips docs that are open (editor-authoritative) or already have a
/// fully parsed body. Cache hydration (`DocumentState::hydrated_from_cache`)
/// leaves `parsed.body = None` so per-doc symbols can populate ahead
/// of parse, but body-dependent passes (the module-call walk in
/// `rebuild_assigned_variable_types_for_dir`, body-walking
/// diagnostics, etc.) need the AST — those get re-parsed. The skip
/// snapshot is taken before the parallel parse, so a buffer opened
/// mid-parse won't be in it; `upsert_document_unless_open` is the
/// backstop that closes that TOCTOU at upsert time.
pub fn parse_and_upsert_files(state: &StateStore, files: &[PathBuf]) -> ParseAndUpsert {
    use rayon::prelude::*;

    let skip: std::collections::HashSet<url::Url> = state
        .documents
        .iter()
        .filter(|e| state.is_open(e.key()) || e.value().parsed.body.is_some())
        .map(|e| e.key().clone())
        .collect();

    let parsed: Vec<DocumentState> = files
        .par_iter()
        .filter_map(|path| {
            let url = path_to_url(path)?;
            if skip.contains(&url) {
                return None;
            }
            let text = std::fs::read_to_string(path).ok()?;
            Some(DocumentState::new(url, &text, 0))
        })
        .collect();

    let parsed_count = parsed.len();
    let mut uris: Vec<url::Url> = parsed.iter().map(|d| d.uri.clone()).collect();
    for doc in parsed {
        state.upsert_document_unless_open(doc);
    }
    // Also include any already-open docs that sit in the same dirs
    // we just scanned — they should be in the publish round too so
    // cross-file aggregates (added-provider, etc.) refresh.
    for f in files {
        if let Some(url) = path_to_url(f) {
            if !uris.contains(&url) && state.documents.contains_key(&url) {
                uris.push(url);
            }
        }
    }

    ParseAndUpsert { uris, parsed_count }
}

/// Run one file's diagnostic compute, containing any panic to THIS file.
///
/// Without this, a panic in one file's diagnostic pass unwinds the whole
/// rayon `par_iter` out of the caller's bulk scan, skipping its
/// scan-completion bookkeeping and wedging every discovered dir
/// permanently mid-scan for the session. The parse layer is
/// panic-guarded (`tfls_parser::safe`); the diagnostic layer is not —
/// so contain it here and drop just the offending file (`None`),
/// letting the scan finish and mark every dir complete.
pub fn catch_file_diag<F: FnOnce() -> Vec<lsp_types::Diagnostic>>(
    uri: &url::Url,
    compute: F,
) -> Option<Vec<lsp_types::Diagnostic>> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(compute)) {
        Ok(diagnostics) => Some(diagnostics),
        Err(_) => {
            tracing::error!(
                uri = %uri,
                "catch_file_diag: diagnostic pass PANICKED; \
                 skipping this file so the scan still completes"
            );
            None
        }
    }
}

/// Recompute the caller-passed unknown-variable map contributed by module
/// calls authored in `dir`. For each module-block argument, decide whether
/// its membership and/or value is apply-time in the CALLER's context; stage
/// per child dir and replace this caller's contribution wholesale (see
/// `StateStore::replace_unknown_module_vars_from_caller`).
///
/// The caller's context unions its OWN cached caller-unknownness, so
/// multi-hop chains (grandparent → parent → child) converge over the
/// successive scan rebuilds that already follow directory scans — no
/// recursion, no cycles.
pub fn rebuild_unknown_module_vars_for_dir(state: &StateStore, dir: &Path) {
    use std::collections::HashMap;
    use tfls_diag::unknown_value::{membership_apply_time, value_apply_time, MetaKind, UnknownCtx};
    use tfls_state::UnknownVarBits;

    fn is_meta_attr(name: &str) -> bool {
        matches!(
            name,
            "source" | "version" | "providers" | "count" | "for_each" | "depends_on"
        )
    }

    let mut caller_inputs = crate::module::module_unknown_inputs_for_dir(state, dir);
    crate::module::fill_unknown_variables(state, dir, &mut caller_inputs);
    let schema_lookup = crate::module::StateStoreSchemaLookup { state };
    let output_cache = crate::module::ModuleOutputCache::default();
    let resolver = crate::module::ModuleOutputResolver {
        state,
        caller_dir: dir.to_path_buf(),
        cache: &output_cache,
    };
    let ctx =
        UnknownCtx::new(&caller_inputs, Some(&schema_lookup)).with_module_outputs(Some(&resolver));

    let mut staged: HashMap<PathBuf, HashMap<String, UnknownVarBits>> = HashMap::new();
    for entry in state.documents.iter() {
        let Ok(doc_path) = entry.key().to_file_path() else {
            continue;
        };
        let Some(parent) = doc_path.parent() else {
            continue;
        };
        if !crate::module::dir_paths_match(parent, dir) {
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
            let Some(source) = entry.value().symbols.module_sources.get(&label).cloned() else {
                continue;
            };
            let Some(child_dir) = crate::module::resolve_module_source(dir, &label, &source) else {
                continue;
            };
            for body_struct in block.body.iter() {
                let Some(attr) = body_struct.as_attribute() else {
                    continue;
                };
                let attr_name = attr.key.as_str();
                if is_meta_attr(attr_name) {
                    continue;
                }
                let membership = membership_apply_time(&attr.value, MetaKind::ForEach, &ctx);
                let value = value_apply_time(&attr.value, &ctx);
                if membership || value {
                    staged.entry(child_dir.clone()).or_default().insert(
                        attr_name.to_string(),
                        UnknownVarBits {
                            membership,
                            value,
                            reason: format!(
                                "caller module \"{label}\" in {} passes an apply-time value",
                                dir.display()
                            ),
                        },
                    );
                }
            }
        }
    }
    state.replace_unknown_module_vars_from_caller(dir.to_path_buf(), staged);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{catch_file_diag, path_to_url};

    #[test]
    fn catch_file_diag_contains_a_panic() {
        // HIGH-4: a panicking diagnostic pass for ONE file must yield None
        // (drop that file) rather than unwind and wedge the whole scan.
        let url = path_to_url(std::path::Path::new("/x.tf")).unwrap();
        assert!(
            catch_file_diag(&url, || panic!("boom")).is_none(),
            "a panic must be contained, returning None"
        );
        assert_eq!(
            catch_file_diag(&url, Vec::new).map(|v| v.len()),
            Some(0),
            "a clean compute passes its result through"
        );
    }
}
