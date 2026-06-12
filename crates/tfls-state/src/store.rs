//! Central state store with secondary indexes for fast symbol lookup.
//!
//! - `documents`: primary source of truth for open document state.
//! - `definitions_by_name`: for each kind/name, the set of defining
//!   locations across the workspace. Supports goto-definition.
//! - `references_by_name`: for each kind/name, the set of reference
//!   locations. Supports find-references.

use std::sync::Arc;

use dashmap::{DashMap, DashSet};
use rustc_hash::FxBuildHasher;
use url::Url;

/// DashMap aliased to use [`FxBuildHasher`]. We don't need
/// HashDoS resistance for these internal indexes — every key is
/// produced by the server itself (URIs / symbol names / provider
/// addresses we parsed) so the speed-up from FxHash dominates.
type FxDashMap<K, V> = DashMap<K, V, FxBuildHasher>;
/// Same rationale for the open-doc URI set.
type FxDashSet<K> = DashSet<K, FxBuildHasher>;
use tfls_core::lock_file::{self, LockFile};
use tfls_core::variable_type::{Primitive, SchemaLookup, VariableType};
use tfls_core::{ProviderAddress, SymbolKind, SymbolLocation};
use tfls_parser::ReferenceKind;
use tfls_schema::{FunctionSignature, FunctionsSchema, ProviderSchema, ProviderSchemas, Schema};

use crate::document::DocumentState;

/// A kind+name pair used as a global index key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolKey {
    pub kind: SymbolKind,
    pub name: String,
}

impl SymbolKey {
    pub fn new(kind: SymbolKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }

    /// Resource/DataSource keys encode both type and instance name as
    /// `<type>.<name>` so a single `SymbolKey` identifies them uniquely.
    pub fn resource(kind: SymbolKind, resource_type: &str, name: &str) -> Self {
        Self::new(kind, format!("{resource_type}.{name}"))
    }
}

/// Plan-time unknownness a caller passes into a child-module variable.
/// Mirror of `tfls-diag`'s `UnknownVarInfo` kept here so `tfls-state`
/// stays independent of the diagnostics crate.
#[derive(Debug, Clone, Default)]
pub struct UnknownVarBits {
    /// Membership (map keys / set elements / length) of the passed value
    /// is apply-time.
    pub membership: bool,
    /// The passed value itself is apply-time.
    pub value: bool,
    /// Human-readable origin (names the caller module + dir).
    pub reason: String,
}

/// Lifecycle state for a workspace directory tracked by the
/// background indexer. Used by [`StateStore::dir_scans`] to
/// distinguish "scan enqueued but not run yet" from "scan finished;
/// peer files are in `documents`". The distinction matters for
/// correctness-sensitive callers (diagnostics, goto-definition) that
/// rely on cross-file symbols being present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirScanState {
    /// A scan job has been enqueued for this directory, but the
    /// worker hasn't finished processing it yet. Peer files in this
    /// dir may not be in the store. Dedupe callers (e.g. "don't
    /// re-queue") accept this state; correctness callers don't.
    Scheduled,
    /// The scan job has run to completion. Every discoverable `.tf`
    /// file in this dir has been parsed and upserted into
    /// [`StateStore::documents`] — cross-file lookups are safe.
    Completed,
}

#[derive(Debug, Default)]
pub struct StateStore {
    pub documents: FxDashMap<Url, DocumentState>,
    pub definitions_by_name: FxDashMap<SymbolKey, Vec<SymbolLocation>>,
    pub references_by_name: FxDashMap<SymbolKey, Vec<SymbolLocation>>,
    /// Provider schemas keyed by [`ProviderAddress`]. Stored as [`Arc`]
    /// so completion/hover handlers can share the data without
    /// cloning the (possibly multi-megabyte) schema contents.
    pub schemas: FxDashMap<ProviderAddress, Arc<ProviderSchema>>,
    /// Built-in function signatures keyed by function name. Shared as
    /// [`Arc`] so signatureHelp doesn't clone descriptions on each lookup.
    pub functions: FxDashMap<String, Arc<FunctionSignature>>,
    /// Runtime configuration updated via `workspace/didChangeConfiguration`.
    pub config: crate::config::ConfigCell,
    /// Directories tracked by the background scanner. Each entry
    /// records the state of that directory's `.tf` file indexing:
    /// [`DirScanState::Scheduled`] (a job has been enqueued; its
    /// files aren't necessarily in the store yet) or
    /// [`DirScanState::Completed`] (the scan has finished and peer
    /// files are guaranteed to be in [`Self::documents`]). Consumers
    /// that need correctness — e.g. "all peer variables are
    /// resolvable" — should gate on `Completed`; consumers that just
    /// need dedupe of scan enqueues — e.g. "don't re-queue this dir"
    /// — check for presence regardless of state.
    pub dir_scans: FxDashMap<std::path::PathBuf, DirScanState>,

    /// Terraform init-root directories (containing a `.terraform/providers/`
    /// subtree) we have fetched schemas from, keyed on the mtime of
    /// `.terraform/providers/` at fetch time. If the current mtime
    /// differs (the user ran `tofu init` after a provider change),
    /// the next check re-enqueues a fetch. Without this, a user who
    /// added a new provider mid-session would never see its schema
    /// load for the rest of the server's lifetime.
    pub fetched_schema_dirs: FxDashMap<std::path::PathBuf, std::time::SystemTime>,

    /// Installed provider VERSION per [`ProviderAddress`]. Populated
    /// alongside [`Self::schemas`] when the plugin-protocol /
    /// CLI-fallback fetch lands. Used by the upgrade-hint
    /// diagnostic to render messages like "available in v4.71.0 —
    /// you're on v4.50.0". Empty when schemas come from a path
    /// that doesn't carry version metadata; the diagnostic falls
    /// back to a hint that doesn't quote the installed version.
    pub installed_provider_versions: FxDashMap<ProviderAddress, String>,

    /// Set to `true` during `initialize` when the client advertises
    /// support for pull-based diagnostics
    /// (`capabilities.textDocument.diagnostic`). When `true` the
    /// server skips push-based `publishDiagnostics` for open
    /// buffers — otherwise nvim (and any other client that stores
    /// both channels separately) ends up with duplicate
    /// diagnostic entries. Default `false` preserves push-only
    /// behaviour for clients that don't do pull.
    pub client_supports_pull_diagnostics: std::sync::atomic::AtomicBool,

    /// True when the client advertised support for
    /// `workspace/diagnostic/refresh` at `initialize`
    /// (`capabilities.workspace.diagnostic.refresh_support`). When
    /// `true` the server can nudge the client to re-pull diagnostics
    /// after an async background scan added new cross-file symbols
    /// that could invalidate previous per-file results. Default
    /// `false` so clients that don't advertise it don't get spurious
    /// requests.
    pub client_supports_diagnostic_refresh: std::sync::atomic::AtomicBool,

    /// URIs currently open in the client (received `didOpen`, no
    /// matching `didClose` yet). Used to distinguish "client will
    /// pull this" (open) from "client will only see this via push"
    /// (unopened workspace files surfaced by bulk scan).
    pub open_docs: FxDashSet<Url>,

    /// Per-target-module-dir cache of variable types inferred from
    /// values flowing INTO the module:
    ///
    /// - tfvars assignments (`*.tfvars`, `*.auto.tfvars`,
    ///   `*.tfvars.json`) in the same directory as the variable
    ///   declarations (root-module case).
    /// - `module "X" { var_name = expr }` attributes from caller
    ///   files in any peer module that has `source = "./X"` or a
    ///   lockfile-resolvable equivalent (child-module case).
    ///
    /// Keyed by the directory containing the variable
    /// declarations (the *target* of the assignment). The inner
    /// `Vec` keeps every observed type so the consumer (e.g. the
    /// type-inference code action) can equality-merge across
    /// multiple call sites / env-specific tfvars files. Values
    /// that resolve to `Any` are filtered out at insertion time.
    pub assigned_variable_types: FxDashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, Vec<tfls_core::variable_type::VariableType>>,
    >,

    /// Plan-time unknownness callers pass into child-module variables:
    /// `child_dir → (caller_dir → (var_name → bits))`. Rebuilt per CALLER
    /// dir by the indexer (next to `assigned_variable_types`); keying the
    /// inner map by caller dir lets each rebuild replace only its own
    /// contribution when several caller dirs feed one shared child.
    /// Consumed by the unknown-value diagnostics when computing a child
    /// dir's documents.
    pub unknown_module_vars: FxDashMap<
        std::path::PathBuf,
        std::collections::HashMap<
            std::path::PathBuf,
            std::collections::HashMap<String, UnknownVarBits>,
        >,
    >,

    /// Per-module-dir cache of the parsed `.terraform.lock.hcl`
    /// file. Populated lazily by [`StateStore::lock_file_for`],
    /// keyed by the module-root directory (the dir that holds
    /// the lock file alongside `.terraform/`). Sentinel "no lock
    /// file present" is encoded by the absence of an entry —
    /// cache misses go through a `metadata()` syscall and
    /// repopulate (or remove) accordingly.
    pub locks: FxDashMap<std::path::PathBuf, Arc<LockFile>>,
    /// Mtime of the lock file at the time it was parsed into
    /// [`Self::locks`]. Used by [`StateStore::lock_file_for`] to
    /// reparse on disk change without depending on the file
    /// watcher having delivered an event yet (cold-read first
    /// access right after `terraform init`).
    pub locks_mtime: FxDashMap<std::path::PathBuf, std::time::SystemTime>,

    /// Serializes the document↔index mutators (`upsert_document`,
    /// `reparse_document`, `remove_document`). The `documents` map and the
    /// `definitions_by_name`/`references_by_name` secondary indexes are
    /// separate DashMaps, so a multi-step mutation across them is only
    /// per-shard atomic. These mutators run concurrently from the worker
    /// task, the file watcher, and `did_change`/`did_save` on
    /// `spawn_blocking`; without this lock, interleaving on the same URI
    /// could leave orphaned index locations or partially-cleared indexes.
    /// Held only for the brief index rebuild — not across I/O.
    index_lock: std::sync::Mutex<()>,
}

impl StateStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record whether the client advertised support for pull
    /// diagnostics (`textDocument/diagnostic`) at `initialize`
    /// time. Call once from the `initialize` handler.
    pub fn set_client_supports_pull_diagnostics(&self, v: bool) {
        self.client_supports_pull_diagnostics
            .store(v, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record whether the client advertised
    /// `workspace.diagnostic.refresh_support` at `initialize`.
    /// Call once from the `initialize` handler.
    pub fn set_client_supports_diagnostic_refresh(&self, v: bool) {
        self.client_supports_diagnostic_refresh
            .store(v, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the server may send `workspace/diagnostic/refresh`
    /// to this client. Read from the indexer's scan-completion
    /// hooks so we don't spam clients that haven't advertised the
    /// capability.
    pub fn client_supports_diagnostic_refresh(&self) -> bool {
        self.client_supports_diagnostic_refresh
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Always returns `false`: the server pushes diagnostics for
    /// every URI, including open buffers. Used to short-circuit pull
    /// mode back when we advertised `diagnosticProvider`. We dropped
    /// that capability (see `crates/tfls-lsp/src/capabilities.rs` —
    /// nvim's dual-namespace render bug), so the previous "skip
    /// push for open buffers under pull-mode clients" logic would
    /// leave open buffers with NO diagnostics at all (server
    /// suppressed push, client never pulls because the capability
    /// isn't advertised). Kept as a function so call sites stay
    /// stable; remove the predicate at every call site once we're
    /// confident the push-only model is stable.
    pub fn should_skip_push_diagnostics(&self, _uri: &Url) -> bool {
        false
    }

    /// Mark a URI as open in the client.
    pub fn mark_open(&self, uri: Url) {
        self.open_docs.insert(uri);
    }

    /// Unmark a URI on `didClose`.
    pub fn mark_closed(&self, uri: &Url) {
        self.open_docs.remove(uri);
    }

    /// Replace the assigned-variable-types entries for `target_dir`
    /// with `assignments` (keyed by variable name → every observed
    /// type for that variable). Wholesale replace rather than merge:
    /// the indexer recomputes from a current snapshot of every
    /// caller / tfvars file each run, so stale entries from a
    /// removed call site shouldn't linger.
    pub fn replace_assigned_variable_types(
        &self,
        target_dir: std::path::PathBuf,
        assignments: std::collections::HashMap<String, Vec<tfls_core::variable_type::VariableType>>,
    ) {
        if assignments.is_empty() {
            self.assigned_variable_types.remove(&target_dir);
            return;
        }
        self.assigned_variable_types.insert(target_dir, assignments);
    }

    /// Look up the merged inferred type for variable `name` declared
    /// in `target_dir`. Reduces every observed assignment via
    /// [`tfls_core::variable_type::merge_types`] so callers passing
    /// same-shape but different-length tuples (or objects with
    /// some fields `Any` from un-resolved chains) still produce a
    /// canonical inferred shape — e.g. `Tuple([string × 6])` and
    /// `Tuple([string × 7])` reduce to `List(string)`.
    ///
    /// Returns `None` only when:
    /// - the dir has no assignments for `name`; OR
    /// - every observation already collapsed to `Any` (no signal).
    pub fn merged_assigned_type(
        &self,
        target_dir: &std::path::Path,
        name: &str,
    ) -> Option<tfls_core::variable_type::VariableType> {
        let entry = self.assigned_variable_types.get(target_dir)?;
        let observations = entry.get(name)?;
        let merged = tfls_core::variable_type::merge_observations(observations)?;
        if matches!(&merged, tfls_core::variable_type::VariableType::Any) {
            None
        } else {
            Some(merged)
        }
    }

    /// Replace `caller_dir`'s contribution to [`Self::unknown_module_vars`]:
    /// `staged` maps each child dir this caller passes apply-time values
    /// into → (var name → bits). Children the caller no longer references
    /// have their `caller_dir` sub-entry removed (empty outer entries are
    /// dropped), so a fixed call site stops flagging on the next rebuild.
    pub fn replace_unknown_module_vars_from_caller(
        &self,
        caller_dir: std::path::PathBuf,
        staged: std::collections::HashMap<
            std::path::PathBuf,
            std::collections::HashMap<String, UnknownVarBits>,
        >,
    ) {
        // Drop this caller's stale sub-entries first.
        let mut emptied: Vec<std::path::PathBuf> = Vec::new();
        for mut entry in self.unknown_module_vars.iter_mut() {
            if staged.contains_key(entry.key()) {
                continue;
            }
            entry.value_mut().remove(&caller_dir);
            if entry.value().is_empty() {
                emptied.push(entry.key().clone());
            }
        }
        for child in emptied {
            self.unknown_module_vars
                .remove_if(&child, |_, v| v.is_empty());
        }
        for (child_dir, vars) in staged {
            self.unknown_module_vars
                .entry(child_dir)
                .or_default()
                .insert(caller_dir.clone(), vars);
        }
    }

    /// Is this URI currently open in any client buffer? Used by
    /// cross-file diagnostic refresh to push fresh state only to
    /// buffers the user can actually see.
    pub fn is_open(&self, uri: &Url) -> bool {
        self.open_docs.contains(uri)
    }
}

/// Resolve a path to its canonical form when possible; fall back
/// to the original `PathBuf` when canonicalisation fails (path
/// doesn't exist, permission denied, etc.). Used to keep lock-
/// file cache keys consistent regardless of whether the caller
/// passed a canonical (watcher / fsevents) or non-canonical
/// (URI parent) path.
fn canonical_or_owned(p: &std::path::Path) -> std::path::PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

impl StateStore {
    /// Read the parsed `.terraform.lock.hcl` for `module_dir`.
    /// Cached after first call; reparsed when the on-disk mtime
    /// changes (so a `terraform init` upgrade is picked up the
    /// next time a handler asks). Returns `None` when the lock
    /// file does not exist — that's the normal case for an
    /// un-initialised workspace.
    ///
    /// Path is canonicalised at the cache layer so symlinked dirs
    /// (macOS `/var → /private/var`) don't produce two cache
    /// entries for the same lock file. Without this, the watcher
    /// (which reports canonical paths via fsevents) and the
    /// inlay-hint / diagnostic call sites (which use URI parents,
    /// often non-canonical) would write under different keys —
    /// `invalidate_lock(canonical)` would miss a non-canonical
    /// entry and stale lock data would persist across user
    /// `terraform init` runs.
    pub fn lock_file_for(&self, module_dir: &std::path::Path) -> Option<Arc<LockFile>> {
        let key = canonical_or_owned(module_dir);
        let lock_path = key.join(".terraform.lock.hcl");
        let mtime = std::fs::metadata(&lock_path)
            .ok()
            .and_then(|m| m.modified().ok());
        let Some(current_mtime) = mtime else {
            // File gone — drop any stale cache entry.
            self.locks.remove(&key);
            self.locks_mtime.remove(&key);
            return None;
        };
        if let Some(cached_mtime) = self.locks_mtime.get(&key) {
            if *cached_mtime == current_mtime {
                if let Some(cached) = self.locks.get(&key) {
                    return Some(Arc::clone(&cached));
                }
            }
        }
        let parsed = lock_file::read_for_module(&key)?;
        let arc = Arc::new(parsed);
        self.locks.insert(key.clone(), Arc::clone(&arc));
        self.locks_mtime.insert(key, current_mtime);
        Some(arc)
    }

    /// Drop any cached lock file for `module_dir`. Called from
    /// the indexer when the file watcher reports a change to
    /// the lock file — next `lock_file_for` call will re-read.
    /// Canonicalises the path so the watcher's canonical-path
    /// invalidation matches caches written under non-canonical
    /// URI-parent keys.
    pub fn invalidate_lock(&self, module_dir: &std::path::Path) {
        let key = canonical_or_owned(module_dir);
        self.locks.remove(&key);
        self.locks_mtime.remove(&key);
    }

    /// Record that a scan has been enqueued for `dir`. Returns
    /// `true` if this is the first time the dir is being tracked —
    /// caller should enqueue the scan job. Returns `false` if the
    /// dir is already `Scheduled` or `Completed`, meaning a scan is
    /// either in flight or has already run; the caller should skip
    /// to avoid redundant work.
    ///
    /// Does NOT overwrite a `Completed` entry — a completed scan's
    /// files are already in the store; re-marking as `Scheduled`
    /// would lie about that.
    pub fn mark_scan_scheduled(&self, dir: std::path::PathBuf) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.dir_scans.entry(dir) {
            Entry::Occupied(_) => false,
            Entry::Vacant(v) => {
                v.insert(DirScanState::Scheduled);
                true
            }
        }
    }

    /// Upgrade `dir`'s scan state to `Completed`. Called by the scan
    /// worker once `scan_files_parallel` has upserted every file in
    /// the directory. Overwrites any prior `Scheduled` entry and
    /// inserts a fresh `Completed` if the dir hadn't been tracked
    /// yet (can happen when the bulk scan discovers a dir that no
    /// `did_open` ever touched).
    pub fn mark_scan_completed(&self, dir: std::path::PathBuf) {
        self.dir_scans.insert(dir, DirScanState::Completed);
    }

    /// True when `dir` is tracked in any state. Use for
    /// dedupe-level checks (don't re-queue).
    pub fn is_scan_tracked(&self, dir: &std::path::Path) -> bool {
        self.dir_scans.contains_key(dir)
    }

    /// True when `dir`'s scan has reached `Completed`. Use for
    /// correctness checks that require peer files to be in the
    /// store.
    pub fn is_scan_completed(&self, dir: &std::path::Path) -> bool {
        self.dir_scans
            .get(dir)
            .map(|v| *v == DirScanState::Completed)
            .unwrap_or(false)
    }

    /// Install a batch of function signatures, replacing any previous set.
    pub fn install_functions(&self, schema: FunctionsSchema) {
        self.functions.clear();
        for (name, sig) in schema.function_signatures {
            self.functions.insert(name, Arc::new(sig));
        }
    }

    /// Merge additional function signatures (e.g. provider-defined
    /// functions) into the existing set without clearing built-ins.
    pub fn merge_functions(
        &self,
        functions: impl IntoIterator<Item = (String, FunctionSignature)>,
    ) {
        for (name, sig) in functions {
            self.functions.insert(name, Arc::new(sig));
        }
    }

    /// Install the entire [`ProviderSchemas`] document into the store,
    /// indexing each provider by its parsed [`ProviderAddress`].
    ///
    /// Entries whose key cannot be parsed as a provider address are
    /// logged and skipped rather than failing the whole batch.
    pub fn install_schemas(&self, schemas: ProviderSchemas) {
        for (raw_key, schema) in schemas.provider_schemas {
            match ProviderAddress::parse(&raw_key) {
                Ok(addr) => {
                    self.schemas.insert(addr, Arc::new(schema));
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    raw_key,
                    "failed to parse provider address, skipping schema"
                ),
            }
        }
    }

    /// Record the installed version for `addr`. Called alongside
    /// `install_schemas` once the provider-protocol fetch knows
    /// which version it spoke to. Used by the upgrade-hint
    /// diagnostic.
    pub fn record_installed_version(&self, addr: ProviderAddress, version: String) {
        self.installed_provider_versions.insert(addr, version);
    }

    /// Look up the installed version recorded for a provider, if
    /// any. `None` when schemas were installed from a source that
    /// didn't carry version metadata (e.g. early CLI fallback).
    pub fn installed_version(&self, addr: &ProviderAddress) -> Option<String> {
        self.installed_provider_versions
            .get(addr)
            .map(|e| e.value().clone())
    }

    /// True when at least one "real" (non-built-in) provider schema is
    /// loaded. The built-in `terraform` provider snapshot is injected at
    /// session start and is always present, so a plain `schemas.is_empty()`
    /// can no longer answer "has this workspace had `terraform init` run?".
    /// Callers that want to surface a "run terraform init" hint should use
    /// this instead.
    pub fn has_real_provider_schemas(&self) -> bool {
        self.schemas.iter().any(|e| !e.key().is_builtin())
    }

    /// Look up a resource schema by its unqualified type name across
    /// all installed providers.
    pub fn find_resource_schema(&self, type_name: &str) -> Option<Arc<ProviderSchema>> {
        self.schemas
            .iter()
            .find(|e| e.value().resource_schemas.contains_key(type_name))
            .map(|e| Arc::clone(e.value()))
    }

    /// Look up a data source schema by its unqualified type name
    /// across all installed providers.
    pub fn find_data_source_schema(&self, type_name: &str) -> Option<Arc<ProviderSchema>> {
        self.schemas
            .iter()
            .find(|e| e.value().data_source_schemas.contains_key(type_name))
            .map(|e| Arc::clone(e.value()))
    }

    /// All known resource type names across all providers.
    pub fn all_resource_types(&self) -> Vec<String> {
        let mut out = Vec::new();
        for entry in self.schemas.iter() {
            out.extend(entry.value().resource_schemas.keys().cloned());
        }
        out.sort();
        out.dedup();
        out
    }

    /// All known data source type names across all providers.
    pub fn all_data_source_types(&self) -> Vec<String> {
        let mut out = Vec::new();
        for entry in self.schemas.iter() {
            out.extend(entry.value().data_source_schemas.keys().cloned());
        }
        out.sort();
        out.dedup();
        out
    }

    /// Find all resources of a given type across all indexed documents.
    pub fn resources_of_type(&self, type_name: &str) -> Vec<String> {
        let mut out = Vec::new();
        for entry in self.documents.iter() {
            for addr in entry.symbols.resources.keys() {
                if addr.resource_type == type_name {
                    out.push(addr.name.clone());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Find all data sources of a given type across all indexed documents.
    pub fn data_sources_of_type(&self, type_name: &str) -> Vec<String> {
        let mut out = Vec::new();
        for entry in self.documents.iter() {
            for addr in entry.symbols.data_sources.keys() {
                if addr.resource_type == type_name {
                    out.push(addr.name.clone());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// All variable names across all indexed documents.
    pub fn all_variable_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for entry in self.documents.iter() {
            out.extend(entry.symbols.variables.keys().cloned());
        }
        out.sort();
        out.dedup();
        out
    }

    /// All local names across all indexed documents.
    pub fn all_local_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for entry in self.documents.iter() {
            out.extend(entry.symbols.locals.keys().cloned());
        }
        out.sort();
        out.dedup();
        out
    }

    /// Get a resource's schema struct directly (attributes + block_types).
    pub fn resource_schema(&self, type_name: &str) -> Option<Schema> {
        self.find_resource_schema(type_name)
            .and_then(|p| p.resource_schemas.get(type_name).cloned())
    }

    /// Get a data source's schema struct directly.
    pub fn data_source_schema(&self, type_name: &str) -> Option<Schema> {
        self.find_data_source_schema(type_name)
            .and_then(|p| p.data_source_schemas.get(type_name).cloned())
    }

    /// Lock the index-maintenance mutex, recovering from poisoning (we
    /// never panic while holding it, so the inner `()` is always valid).
    fn lock_indexes(&self) -> std::sync::MutexGuard<'_, ()> {
        self.index_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Insert (or replace) a document and rebuild its indexes.
    pub fn upsert_document(&self, doc: DocumentState) {
        let _guard = self.lock_indexes();
        let uri = doc.uri.clone();
        self.remove_from_indexes(&uri);
        self.add_to_indexes(&doc);
        self.documents.insert(uri, doc);
    }

    /// Re-analyse an existing document in place and refresh its indexes.
    pub fn reparse_document(&self, uri: &Url) {
        let _guard = self.lock_indexes();
        self.remove_from_indexes(uri);
        if let Some(mut doc) = self.documents.get_mut(uri) {
            doc.reparse();
            self.add_to_indexes(&doc);
        }
    }

    /// Remove a document from the store and from indexes.
    pub fn remove_document(&self, uri: &Url) -> Option<DocumentState> {
        let _guard = self.lock_indexes();
        self.remove_from_indexes(uri);
        self.documents.remove(uri).map(|(_, d)| d)
    }

    fn add_to_indexes(&self, doc: &DocumentState) {
        for (name, sym) in &doc.symbols.variables {
            self.definitions_by_name
                .entry(SymbolKey::new(SymbolKind::Variable, name))
                .or_default()
                .push(sym.location.clone());
        }
        for (name, sym) in &doc.symbols.locals {
            self.definitions_by_name
                .entry(SymbolKey::new(SymbolKind::Local, name))
                .or_default()
                .push(sym.location.clone());
        }
        for (name, sym) in &doc.symbols.outputs {
            self.definitions_by_name
                .entry(SymbolKey::new(SymbolKind::Output, name))
                .or_default()
                .push(sym.location.clone());
        }
        for (name, sym) in &doc.symbols.modules {
            self.definitions_by_name
                .entry(SymbolKey::new(SymbolKind::Module, name))
                .or_default()
                .push(sym.location.clone());
        }
        for (addr, sym) in &doc.symbols.resources {
            self.definitions_by_name
                .entry(SymbolKey::resource(
                    SymbolKind::Resource,
                    &addr.resource_type,
                    &addr.name,
                ))
                .or_default()
                .push(sym.location.clone());
        }
        for (addr, sym) in &doc.symbols.data_sources {
            self.definitions_by_name
                .entry(SymbolKey::resource(
                    SymbolKind::DataSource,
                    &addr.resource_type,
                    &addr.name,
                ))
                .or_default()
                .push(sym.location.clone());
        }

        for r in &doc.references {
            let key = reference_key(&r.kind);
            self.references_by_name
                .entry(key)
                .or_default()
                .push(r.location.clone());
        }
    }

    fn remove_from_indexes(&self, uri: &Url) {
        let to_remove = if let Some(doc) = self.documents.get(uri) {
            collect_doc_keys(&doc)
        } else {
            return;
        };

        for key in &to_remove.definitions {
            if let Some(mut entry) = self.definitions_by_name.get_mut(key) {
                entry.retain(|loc| loc.uri != *uri);
            }
        }
        for key in &to_remove.references {
            if let Some(mut entry) = self.references_by_name.get_mut(key) {
                entry.retain(|loc| loc.uri != *uri);
            }
        }
        self.definitions_by_name.retain(|_, v| !v.is_empty());
        self.references_by_name.retain(|_, v| !v.is_empty());
    }
}

struct DocKeys {
    definitions: Vec<SymbolKey>,
    references: Vec<SymbolKey>,
}

fn collect_doc_keys(doc: &DocumentState) -> DocKeys {
    let mut definitions = Vec::new();
    for name in doc.symbols.variables.keys() {
        definitions.push(SymbolKey::new(SymbolKind::Variable, name));
    }
    for name in doc.symbols.locals.keys() {
        definitions.push(SymbolKey::new(SymbolKind::Local, name));
    }
    for name in doc.symbols.outputs.keys() {
        definitions.push(SymbolKey::new(SymbolKind::Output, name));
    }
    for name in doc.symbols.modules.keys() {
        definitions.push(SymbolKey::new(SymbolKind::Module, name));
    }
    for addr in doc.symbols.resources.keys() {
        definitions.push(SymbolKey::resource(
            SymbolKind::Resource,
            &addr.resource_type,
            &addr.name,
        ));
    }
    for addr in doc.symbols.data_sources.keys() {
        definitions.push(SymbolKey::resource(
            SymbolKind::DataSource,
            &addr.resource_type,
            &addr.name,
        ));
    }

    let references = doc
        .references
        .iter()
        .map(|r| reference_key(&r.kind))
        .collect();
    DocKeys {
        definitions,
        references,
    }
}

pub fn reference_key(kind: &ReferenceKind) -> SymbolKey {
    match kind {
        ReferenceKind::Variable { name } => SymbolKey::new(SymbolKind::Variable, name),
        ReferenceKind::Local { name } => SymbolKey::new(SymbolKind::Local, name),
        ReferenceKind::Module { name } => SymbolKey::new(SymbolKind::Module, name),
        ReferenceKind::Resource {
            resource_type,
            name,
        } => SymbolKey::resource(SymbolKind::Resource, resource_type, name),
        ReferenceKind::DataSource {
            resource_type,
            name,
        } => SymbolKey::resource(SymbolKind::DataSource, resource_type, name),
    }
}

impl SchemaLookup for StateStore {
    fn resource_attr(&self, resource_type: &str, attr: &str) -> Option<VariableType> {
        let schema = self.resource_schema(resource_type)?;
        let attr_schema = schema.block.attributes.get(attr)?;
        let raw = attr_schema.r#type.as_ref()?;
        Some(schema_type_to_variable_type(raw))
    }

    fn data_source_attr(&self, type_name: &str, attr: &str) -> Option<VariableType> {
        let schema = self.data_source_schema(type_name)?;
        let attr_schema = schema.block.attributes.get(attr)?;
        let raw = attr_schema.r#type.as_ref()?;
        Some(schema_type_to_variable_type(raw))
    }
}

/// Convert Terraform's JSON-encoded type representation (sonic_rs
/// `Value`) into a [`VariableType`]. Supported shapes:
///
/// - Primitive name string: `"string"` / `"number"` / `"bool"` /
///   `"dynamic"` (→ `Any`).
/// - 2-element array: `["list", T]`, `["set", T]`, `["map", T]`.
/// - 2-element array: `["object", { name: T, … }]`.
/// - 2-element array: `["tuple", [T, T, …]]`.
/// - Anything else → `Any` (the safe fallback that lets downstream
///   inference keep moving).
pub fn schema_type_to_variable_type(value: &sonic_rs::Value) -> VariableType {
    use sonic_rs::{JsonContainerTrait, JsonValueTrait};
    if let Some(s) = value.as_str() {
        return match s {
            "string" => VariableType::Primitive(Primitive::String),
            "number" => VariableType::Primitive(Primitive::Number),
            "bool" => VariableType::Primitive(Primitive::Bool),
            _ => VariableType::Any,
        };
    }
    let Some(arr) = value.as_array() else {
        return VariableType::Any;
    };
    let head = arr.first().and_then(|v| v.as_str());
    let tail = arr.get(1);
    match (head, tail) {
        (Some("list"), Some(t)) => VariableType::List(Box::new(schema_type_to_variable_type(t))),
        (Some("set"), Some(t)) => VariableType::Set(Box::new(schema_type_to_variable_type(t))),
        (Some("map"), Some(t)) => VariableType::Map(Box::new(schema_type_to_variable_type(t))),
        (Some("tuple"), Some(t)) => {
            if let Some(items) = t.as_array() {
                VariableType::Tuple(items.iter().map(schema_type_to_variable_type).collect())
            } else {
                VariableType::Any
            }
        }
        (Some("object"), Some(t)) => {
            if let Some(obj) = t.as_object() {
                let mut fields = std::collections::BTreeMap::new();
                for (k, v) in obj.iter() {
                    fields.insert(k.to_string(), schema_type_to_variable_type(v));
                }
                VariableType::Object(fields)
            } else {
                VariableType::Any
            }
        }
        _ => VariableType::Any,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Url {
        Url::parse(s).expect("valid url")
    }

    #[test]
    fn new_store_is_empty() {
        let store = StateStore::new();
        assert_eq!(store.documents.len(), 0);
        assert_eq!(store.definitions_by_name.len(), 0);
    }

    #[test]
    fn upsert_indexes_variables() {
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        store.upsert_document(DocumentState::new(u.clone(), r#"variable "region" {}"#, 1));

        let key = SymbolKey::new(SymbolKind::Variable, "region");
        let entry = store
            .definitions_by_name
            .get(&key)
            .expect("variable should be indexed");
        assert_eq!(entry.len(), 1);
        assert_eq!(entry[0].uri, u);
    }

    #[test]
    fn upsert_indexes_references() {
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        store.upsert_document(DocumentState::new(
            u.clone(),
            r#"output "x" { value = var.region }"#,
            1,
        ));

        let key = SymbolKey::new(SymbolKind::Variable, "region");
        let entry = store
            .references_by_name
            .get(&key)
            .expect("reference should be indexed");
        assert_eq!(entry.len(), 1);
    }

    #[test]
    fn remove_clears_indexes() {
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        store.upsert_document(DocumentState::new(u.clone(), r#"variable "region" {}"#, 1));
        assert_eq!(store.definitions_by_name.len(), 1);

        store.remove_document(&u);
        assert_eq!(store.definitions_by_name.len(), 0);
        assert_eq!(store.documents.len(), 0);
    }

    #[test]
    fn install_schemas_indexes_providers() {
        let schemas: ProviderSchemas = sonic_rs::from_str(
            r#"{
                "format_version": "1.0",
                "provider_schemas": {
                    "registry.terraform.io/hashicorp/aws": {
                        "provider": { "version": 0, "block": {} },
                        "resource_schemas": {
                            "aws_instance": { "version": 1, "block": {} }
                        },
                        "data_source_schemas": {
                            "aws_ami": { "version": 0, "block": {} }
                        }
                    }
                }
            }"#,
        )
        .expect("parse");

        let store = StateStore::new();
        store.install_schemas(schemas);

        let addr = ProviderAddress::hashicorp("aws");
        assert!(store.schemas.contains_key(&addr));

        assert!(store.resource_schema("aws_instance").is_some());
        assert!(store.data_source_schema("aws_ami").is_some());
        assert!(store.resource_schema("nonexistent").is_none());

        let resources = store.all_resource_types();
        assert_eq!(resources, vec!["aws_instance".to_string()]);
    }

    #[test]
    fn builtin_provider_schema_resolves_and_is_not_counted_as_real() {
        let schemas: ProviderSchemas = sonic_rs::from_str(
            r#"{
                "format_version": "1.0",
                "provider_schemas": {
                    "terraform.io/builtin/terraform": {
                        "provider": { "version": 0, "block": {} },
                        "resource_schemas": {
                            "terraform_data": { "version": 0, "block": {} }
                        },
                        "data_source_schemas": {
                            "terraform_remote_state": { "version": 0, "block": {} }
                        }
                    }
                }
            }"#,
        )
        .expect("parse");

        let store = StateStore::new();
        store.install_schemas(schemas);

        // Built-in types resolve like any other schema...
        assert!(store
            .find_data_source_schema("terraform_remote_state")
            .is_some());
        assert!(store.find_resource_schema("terraform_data").is_some());
        // ...but the built-in snapshot doesn't count as a "real" fetched
        // provider, so the "run terraform init" hint still applies.
        assert!(!store.has_real_provider_schemas());

        // Adding an actual provider flips it.
        let aws: ProviderSchemas = sonic_rs::from_str(
            r#"{ "format_version": "1.0", "provider_schemas": {
                "registry.terraform.io/hashicorp/aws": {
                    "provider": { "version": 0, "block": {} },
                    "resource_schemas": { "aws_instance": { "version": 1, "block": {} } }
                } } }"#,
        )
        .expect("parse");
        store.install_schemas(aws);
        assert!(store.has_real_provider_schemas());
    }

    #[test]
    fn reparse_refreshes_indexes() {
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        store.upsert_document(DocumentState::new(u.clone(), r#"variable "old" {}"#, 1));
        assert!(store
            .definitions_by_name
            .contains_key(&SymbolKey::new(SymbolKind::Variable, "old")));

        if let Some(mut doc) = store.documents.get_mut(&u) {
            doc.rope = ropey::Rope::from_str(r#"variable "new" {}"#);
        }
        store.reparse_document(&u);

        assert!(!store
            .definitions_by_name
            .contains_key(&SymbolKey::new(SymbolKind::Variable, "old")));
        assert!(store
            .definitions_by_name
            .contains_key(&SymbolKey::new(SymbolKind::Variable, "new")));
    }

    // --- dir_scans state machine ------------------------------------

    #[test]
    fn mark_scan_scheduled_is_idempotent() {
        let store = StateStore::new();
        let d = std::path::PathBuf::from("/module/a");
        assert!(
            store.mark_scan_scheduled(d.clone()),
            "first mark should return true"
        );
        assert!(
            !store.mark_scan_scheduled(d.clone()),
            "second mark should return false"
        );
        assert!(store.is_scan_tracked(&d));
        assert!(!store.is_scan_completed(&d));
    }

    #[test]
    fn mark_scan_completed_upgrades_scheduled() {
        let store = StateStore::new();
        let d = std::path::PathBuf::from("/module/a");
        store.mark_scan_scheduled(d.clone());
        store.mark_scan_completed(d.clone());
        assert!(store.is_scan_completed(&d));
        // Another schedule attempt must NOT flip back — Completed
        // should be a sticky terminal state for the correctness
        // reading; the peer files are already in the store.
        assert!(
            !store.mark_scan_scheduled(d.clone()),
            "scheduling an already-completed dir must no-op"
        );
        assert!(
            store.is_scan_completed(&d),
            "Completed state must not regress to Scheduled"
        );
    }

    #[test]
    fn mark_scan_completed_without_prior_schedule() {
        // Bulk scan can mark a dir Completed directly without a
        // prior Scheduled entry (the discovery + scan happen
        // atomically from the point of view of any outside caller).
        let store = StateStore::new();
        let d = std::path::PathBuf::from("/module/a");
        store.mark_scan_completed(d.clone());
        assert!(store.is_scan_tracked(&d));
        assert!(store.is_scan_completed(&d));
    }

    #[test]
    fn untracked_dir_is_neither_tracked_nor_completed() {
        let store = StateStore::new();
        let d = std::path::PathBuf::from("/module/never-seen");
        assert!(!store.is_scan_tracked(&d));
        assert!(!store.is_scan_completed(&d));
    }

    // --- should_skip_push_diagnostics invariant --------------------
    //
    // After dropping `diagnosticProvider` from server capabilities,
    // pull diagnostics never fire on the wire. Push has to cover
    // every URI, including open buffers — otherwise open buffers
    // get NO diagnostics at all (server suppresses push, client
    // never pulls). The function is now a constant `false`; this
    // test pins it so re-introducing the skip without restoring
    // pull mode fails loudly.

    // --- lock_file_for cache --------------------------------------

    #[test]
    fn lock_file_for_returns_none_when_missing() {
        let store = StateStore::new();
        let dir = std::env::temp_dir().join(format!(
            "tfls-store-locktest-{}",
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(store.lock_file_for(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_file_for_caches_and_invalidates() {
        let store = StateStore::new();
        let dir = std::env::temp_dir().join(format!(
            "tfls-store-locktest-cache-{}",
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".terraform.lock.hcl");
        std::fs::write(
            &path,
            r#"provider "registry.terraform.io/hashicorp/aws" { version = "5.50.0" }"#,
        )
        .unwrap();

        let first = store.lock_file_for(&dir).expect("present");
        assert_eq!(first.len(), 1);
        // Cached: same Arc reference identity on second call.
        let second = store.lock_file_for(&dir).expect("present");
        assert!(Arc::ptr_eq(&first, &second), "second call must hit cache");

        // Invalidate, then verify cache repopulates after the syscall.
        store.invalidate_lock(&dir);
        let third = store.lock_file_for(&dir).expect("present");
        assert_eq!(third.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_file_for_drops_cache_when_file_removed() {
        let store = StateStore::new();
        let dir = std::env::temp_dir().join(format!(
            "tfls-store-locktest-removed-{}",
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".terraform.lock.hcl");
        std::fs::write(
            &path,
            r#"provider "registry.terraform.io/hashicorp/aws" { version = "5.50.0" }"#,
        )
        .unwrap();
        assert!(store.lock_file_for(&dir).is_some());
        std::fs::remove_file(&path).unwrap();
        assert!(store.lock_file_for(&dir).is_none());
        assert!(!store.locks.contains_key(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_skip_push_in_push_only_mode() {
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        // All four pre-fix permutations must now return false.
        assert!(!store.should_skip_push_diagnostics(&u));

        store.mark_open(u.clone());
        assert!(!store.should_skip_push_diagnostics(&u));

        store.set_client_supports_pull_diagnostics(true);
        assert!(!store.should_skip_push_diagnostics(&u));

        store.mark_closed(&u);
        assert!(!store.should_skip_push_diagnostics(&u));
    }
}
