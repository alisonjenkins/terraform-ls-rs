//! Synchronous indexing bookkeeping shared by the LSP server's
//! background indexer and the (future) standalone lint CLI: sizing
//! the rayon pool used for CPU-bound indexing work, and populating a
//! [`StateStore`] from files on disk. The async job queue, file
//! watcher, and diagnostic-publish plumbing stay in `tfls-lsp` — this
//! module is transport-free.

use std::path::{Path, PathBuf};

use tfls_state::{DocumentState, StateStore};
use tfls_walker::discover_terraform_files_in_dir;

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
        // Never touch an OPEN doc — the editor's buffer is authoritative
        // (overwriting from disk reverts unsaved edits + resets the
        // version). For CLOSED docs: skip if already fully parsed; cache
        // hydration (`DocumentState::hydrated_from_cache`) leaves
        // `parsed.body = None`, so overwrite those with a freshly-parsed
        // `DocumentState::new` so body-dependent passes see the AST.
        if state.is_open(&url) {
            continue;
        }
        if let Some(doc) = state.documents.get(&url) {
            if doc.parsed.body.is_some() {
                continue;
            }
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
        if state.upsert_document_unless_open(DocumentState::new(url, &text, 0)) {
            indexed += 1;
        }
    }
    tracing::debug!(
        dir = %dir.display(),
        indexed,
        "sync module index: complete"
    );
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
///
/// Panic isolation lives in [`tfls_parser::safe`], not here:
/// `parse_tfvars` and `parse_value_shape` walk the hcl-edit AST
/// produced by `tfls_parser::parse_body`, which catches any
/// upstream parser panic at the source. So the indexer can call
/// these directly without its own `catch_unwind`.
/// Derive the element type of a `for_each` collection so
/// `each.value` can resolve.
///
/// - `Map(T)` / `List(T)` / `Set(T)` → `T`.
/// - `Object({k → V, …})` with all values equal → that shared `V`.
///   (This covers the common dict-of-records pattern that Terraform
///   accepts as a `for_each` value.)
/// - `Object({…})` with heterogeneous values → return the object
///   itself; `each.value.<field>` will drill in via
///   [`tfls_core::variable_type::drill_into_object`], which returns
///   the field's type when present and `Any` when absent. Drill
///   semantics that good enough for the common
///   `each.value.bucket_name = string` pattern even when other
///   sites add extra fields.
/// - Anything else → `None` (we don't claim to know).
fn element_type_of(
    collection: &tfls_core::variable_type::VariableType,
) -> Option<tfls_core::variable_type::VariableType> {
    use tfls_core::variable_type::VariableType;
    match collection {
        VariableType::Map(inner) | VariableType::List(inner) | VariableType::Set(inner) => {
            Some((**inner).clone())
        }
        VariableType::Object(fields) => {
            if fields.is_empty() {
                return None;
            }
            // Collapse to a per-field "best-effort" object: for each
            // field name appearing in any of the dict values, infer
            // the type from the first value that has it. Keeps
            // `each.value.bucket_name → string` working even when
            // some entries omit the field.
            let mut entries: Vec<&std::collections::BTreeMap<String, VariableType>> = Vec::new();
            for v in fields.values() {
                if let VariableType::Object(inner) = v {
                    entries.push(inner);
                } else {
                    // Non-object value — fall back to direct inner
                    // type if all are equal.
                    let mut iter = fields.values();
                    let first = iter.next()?.clone();
                    return if iter.all(|x| x == &first) {
                        Some(first)
                    } else {
                        None
                    };
                }
            }
            let mut merged: std::collections::BTreeMap<String, VariableType> =
                std::collections::BTreeMap::new();
            for obj in &entries {
                for (k, v) in obj.iter() {
                    if !merged.contains_key(k) {
                        merged.insert(k.clone(), v.clone());
                    }
                }
            }
            if merged.is_empty() {
                None
            } else {
                Some(VariableType::Object(merged))
            }
        }
        _ => None,
    }
}

/// `SchemaLookup` that combines `StateStore`'s schema-aware
/// resource / data-source attribute resolution with caller-side
/// variable / local / module-output context. Used by section 2 of
/// the rebuild loop so a module-call attribute like
/// `name = var.account_number` resolves through the caller's
/// `variable "account_number" { type = string }` declaration to
/// `Primitive(String)` instead of collapsing to `Any`.
///
/// Aggregates symbols across EVERY indexed `.tf` file whose parent
/// directory matches `caller_dir`, not just the single doc that
/// contains the module-call block. Real Terraform stacks split
/// `variables.tf` from the call sites (`apigw.tf`, `route53.tf`,
/// …); a caller doc's own `SymbolTable` rarely declares the
/// variables it references.
struct CallerScopedLookup<'a> {
    state: &'a StateStore,
    caller_dir: &'a Path,
    /// Type of `each.value` when this lookup is being used inside
    /// a `for_each = …` module block. `None` for module blocks
    /// without `for_each`.
    each_value: Option<tfls_core::variable_type::VariableType>,
}

impl CallerScopedLookup<'_> {
    /// Walk every indexed doc whose parent dir is the caller's,
    /// invoking `visit` on its `SymbolTable`. Stops early if the
    /// visitor returns `Some`.
    fn with_caller_dir_symbols<R, F>(&self, mut visit: F) -> Option<R>
    where
        F: FnMut(&tfls_core::SymbolTable) -> Option<R>,
    {
        for entry in self.state.documents.iter() {
            let Ok(p) = entry.key().to_file_path() else {
                continue;
            };
            if p.parent() != Some(self.caller_dir) {
                continue;
            }
            if let Some(r) = visit(&entry.value().symbols) {
                return Some(r);
            }
        }
        None
    }
}

impl tfls_core::variable_type::SchemaLookup for CallerScopedLookup<'_> {
    fn resource_attr(
        &self,
        resource_type: &str,
        attr: &str,
    ) -> Option<tfls_core::variable_type::VariableType> {
        self.state.resource_attr(resource_type, attr)
    }
    fn data_source_attr(
        &self,
        type_name: &str,
        attr: &str,
    ) -> Option<tfls_core::variable_type::VariableType> {
        self.state.data_source_attr(type_name, attr)
    }
    fn variable_type(&self, name: &str) -> Option<tfls_core::variable_type::VariableType> {
        // Prefer the declared `type = …` first; fall back to the
        // shape inferred from `default = …` so a typeless variable
        // with `default = "x"` still resolves. Walks every peer
        // doc in the caller's dir — variables are typically split
        // into `variables.tf` while module calls live elsewhere.
        self.with_caller_dir_symbols(|sym| {
            sym.variable_types
                .get(name)
                .cloned()
                .or_else(|| sym.variable_defaults.get(name).cloned())
        })
    }
    fn local_shape(&self, name: &str) -> Option<tfls_core::variable_type::VariableType> {
        // Recompute the local's shape AT QUERY TIME against the
        // current schema lookup. The cached `local_shapes` on the
        // SymbolTable was populated at parse time via the
        // schema-free `parse_value_shape`, so any `aws_X.attr`
        // inside the local's value collapsed to `Any`. Walking the
        // body fresh — with `self` as the SchemaLookup — picks up
        // resource/data attribute resolution + recursive var/local
        // chains.
        for entry in self.state.documents.iter() {
            let Ok(p) = entry.key().to_file_path() else {
                continue;
            };
            if p.parent() != Some(self.caller_dir) {
                continue;
            }
            let Some(body) = entry.value().parsed.body.as_ref() else {
                continue;
            };
            for s in body.iter() {
                let Some(block) = s.as_block() else { continue };
                if block.ident.as_str() != "locals" {
                    continue;
                }
                for sub in block.body.iter() {
                    let Some(attr) = sub.as_attribute() else {
                        continue;
                    };
                    if attr.key.as_str() != name {
                        continue;
                    }
                    return Some(tfls_core::variable_type::parse_value_shape_with_schema(
                        &attr.value,
                        self,
                    ));
                }
            }
        }
        None
    }
    fn each_value(&self) -> Option<tfls_core::variable_type::VariableType> {
        self.each_value.clone()
    }
    fn module_output(
        &self,
        module_name: &str,
        output_name: &str,
    ) -> Option<tfls_core::variable_type::VariableType> {
        // 1. Find the source for `module "<name>" {}` in any peer
        //    doc of the caller's dir.
        let source =
            self.with_caller_dir_symbols(|sym| sym.module_sources.get(module_name).cloned())?;
        // 2. Resolve to the child module directory.
        let child_dir =
            crate::module::resolve_module_source(self.caller_dir, module_name, &source)?;
        // 3. Walk indexed docs for that child dir; find an
        //    `output "<output>" { value = … }` block; infer the
        //    value's shape.
        for doc in self.state.documents.iter() {
            let Ok(p) = doc.key().to_file_path() else {
                continue;
            };
            if p.parent() != Some(&child_dir) {
                continue;
            }
            let Some(body) = doc.value().parsed.body.as_ref() else {
                continue;
            };
            for s in body.iter() {
                let Some(block) = s.as_block() else { continue };
                if block.ident.as_str() != "output" {
                    continue;
                }
                let label = match block.labels.first()? {
                    hcl_edit::structure::BlockLabel::String(s) => s.value().to_string(),
                    hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
                };
                if label != output_name {
                    continue;
                }
                for sub in block.body.iter() {
                    let Some(attr) = sub.as_attribute() else {
                        continue;
                    };
                    if attr.key.as_str() != "value" {
                        continue;
                    }
                    // Recurse with a CHILD-scoped lookup so that an
                    // output value referencing the child module's
                    // own `local.X` / `var.X` / `module.Y.Z` chains
                    // resolves correctly. Child outputs aren't
                    // limited to plain resource traversals — many
                    // wrap a `local.<name>` indirection over the
                    // actual resource attr (`output "oidc_issuer"
                    // { value = local.oidc_issuer_url }`).
                    let child_scope = CallerScopedLookup {
                        state: self.state,
                        caller_dir: &child_dir,
                        each_value: None,
                    };
                    return Some(tfls_core::variable_type::parse_value_shape_with_schema(
                        &attr.value,
                        &child_scope,
                    ));
                }
            }
        }
        None
    }
}

pub fn rebuild_assigned_variable_types_for_dir(state: &StateStore, dir: &Path) {
    use std::collections::HashMap;
    use tfls_core::variable_type::{parse_value_shape_with_schema, VariableType};

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

    // 1. Tfvars attributable to `dir` → assignments target `dir`.
    //
    // Includes `dir`'s own `*.tfvars` AND any tfvars under `dir` that
    // sit in a "tfvars-only" subdir (no `.tf` of its own — common
    // env-split layouts like `params/nonprod/params.tfvars`). Sibling
    // module dirs are skipped: their tfvars belong to them, not us.
    // See `tfls_walker::discover_tfvars_attributable_to` for the full
    // attribution rule.
    if let Ok(tfvars) = tfls_walker::discover_tfvars_attributable_to(dir) {
        let mut for_dir: HashMap<String, Vec<VariableType>> = HashMap::new();
        for path in &tfvars {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for (name, ty) in tfls_parser::parse_tfvars(&text) {
                for_dir.entry(name).or_default().push(ty);
            }
        }
        if !tfvars.is_empty() || !for_dir.is_empty() {
            tracing::info!(
                dir = %dir.display(),
                tfvars_count = tfvars.len(),
                names = ?for_dir.keys().collect::<Vec<_>>(),
                "rebuild_assigned_variable_types: section 1 (tfvars) staged",
            );
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
            // Walk each attribute of the module block; infer the
            // type of its RHS; stage under the child dir.
            //
            // Caller-scoped lookup: extends the state's
            // resource/data-source schema lookup with the CALLER
            // doc's `variable_types` + `local_shapes` so traversal
            // resolution covers `var.X` / `local.X` / `module.X.Y`
            // references the caller passes in. Without this, every
            // `attr = var.X` collapses to `Any` and we lose the
            // type info even though the caller's variable block
            // declares it.
            // Resolve `for_each = …` in the caller's module block,
            // if present. The collection's element type becomes
            // `each.value`'s type, which lets attributes like
            // `bucket_name = each.value.bucket_name` resolve to a
            // concrete shape instead of `Any`.
            let mut each_value: Option<tfls_core::variable_type::VariableType> = None;
            for sub in block.body.iter() {
                let Some(attr) = sub.as_attribute() else {
                    continue;
                };
                if attr.key.as_str() != "for_each" {
                    continue;
                }
                // Use a caller-scope lookup WITHOUT each_value for
                // the for_each expression itself — `each` is only
                // valid inside the block's body, not in the
                // for_each value.
                let scope = CallerScopedLookup {
                    state,
                    caller_dir: dir,
                    each_value: None,
                };
                let collection = parse_value_shape_with_schema(&attr.value, &scope);
                each_value = element_type_of(&collection);
                break;
            }
            let lookup = CallerScopedLookup {
                state,
                caller_dir: dir,
                each_value,
            };
            let bucket = staged.entry(child_dir).or_default();
            for body_struct in block.body.iter() {
                let Some(attr) = body_struct.as_attribute() else {
                    continue;
                };
                let attr_name = attr.key.as_str();
                if is_meta_attr(attr_name) {
                    continue;
                }
                let ty = parse_value_shape_with_schema(&attr.value, &lookup);
                if matches!(&ty, VariableType::Any) {
                    continue;
                }
                // Empty `Tuple([])` / `Object({})` were previously
                // dropped here as "too ambiguous." That hides the
                // signal entirely from the inference map, so the
                // declared `list(any)` / `map(any)` case can never
                // satisfy the assignment check downstream. Stage
                // them — the code-action's `is_actionable_inference`
                // still filters empties before suggesting a literal,
                // but other consumers (assignment-vs-declared
                // diagnostics) get to see the data.
                bucket.entry(attr_name.to_string()).or_default().push(ty);
            }
        }
    }

    for (target_dir, assignments) in staged {
        state.replace_assigned_variable_types(target_dir, assignments);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{catch_file_diag, index_module_dir_sync, path_to_url};
    use std::fs;
    use std::path::PathBuf;
    use tfls_state::StateStore;
    use url::Url;

    fn tmp_dir(label: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("tfls-engine-index-{label}-{nanos}"));
        let _ = fs::create_dir_all(&dir);
        dir
    }

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

    #[test]
    fn index_module_dir_sync_reads_parses_upserts() {
        let dir = tmp_dir("sync-reads-parses-upserts");
        fs::write(dir.join("variables.tf"), "variable \"region\" {}\n").unwrap();
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
    fn index_module_dir_sync_never_clobbers_open_doc_even_without_ast() {
        // An OPEN buffer with no AST (body=None) — e.g. cache-hydrated, or a
        // recovered/panicked parse. The OLD `body.is_some()` guard would
        // overwrite it from disk (reverting the user's edits + resetting the
        // version); the `is_open` guard must preserve it. This is the precise
        // disk-clobber that caused "out of sync with disk".
        let dir = tmp_dir("sync-open-no-ast");
        let path = dir.join("main.tf");
        fs::write(&path, "variable \"DISK\" {}\n").unwrap();

        let store = StateStore::new();
        let uri = Url::from_file_path(&path).unwrap();
        store.upsert_document(tfls_state::DocumentState::hydrated_from_cache(
            uri.clone(),
            "variable \"BUFFER\" {}\n",
            tfls_core::SymbolTable::default(),
            Vec::new(),
        ));
        store.mark_open(uri.clone());
        assert!(
            store.documents.get(&uri).unwrap().parsed.body.is_none(),
            "precondition: open doc has no AST"
        );

        index_module_dir_sync(&store, &dir);

        let doc = store.documents.get(&uri).unwrap();
        assert!(
            doc.text().contains("BUFFER"),
            "open buffer must survive disk reindex; got: {}",
            doc.text()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_module_dir_sync_tolerates_missing_dir() {
        // Nonexistent directory must NOT panic. `discover_*`
        // returns an error that we log and continue past.
        let store = StateStore::new();
        let dir = std::env::temp_dir().join("tfls-engine-index-does-not-exist");
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
