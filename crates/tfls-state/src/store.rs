//! Central state store with secondary indexes for fast symbol lookup.
//!
//! - `documents`: primary source of truth for open document state.
//! - `definitions_by_name`: for each kind/name, the set of defining
//!   locations across the workspace. Supports goto-definition.
//! - `references_by_name`: for each kind/name, the set of reference
//!   locations. Supports find-references.

use std::sync::Arc;

use dashmap::DashMap;
use lsp_types::Url;
use tfls_core::{ProviderAddress, SymbolKind, SymbolLocation};
use tfls_parser::ReferenceKind;
use tfls_schema::{
    FunctionSignature, FunctionsSchema, ProviderSchema, ProviderSchemas, Schema,
};

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
    pub documents: DashMap<Url, DocumentState>,
    pub definitions_by_name: DashMap<SymbolKey, Vec<SymbolLocation>>,
    pub references_by_name: DashMap<SymbolKey, Vec<SymbolLocation>>,
    /// Provider schemas keyed by [`ProviderAddress`]. Stored as [`Arc`]
    /// so completion/hover handlers can share the data without
    /// cloning the (possibly multi-megabyte) schema contents.
    pub schemas: DashMap<ProviderAddress, Arc<ProviderSchema>>,
    /// Built-in function signatures keyed by function name. Shared as
    /// [`Arc`] so signatureHelp doesn't clone descriptions on each lookup.
    pub functions: DashMap<String, Arc<FunctionSignature>>,
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
    pub dir_scans: dashmap::DashMap<std::path::PathBuf, DirScanState>,

    /// Terraform init-root directories (containing a `.terraform/providers/`
    /// subtree) we have fetched schemas from, keyed on the mtime of
    /// `.terraform/providers/` at fetch time. If the current mtime
    /// differs (the user ran `tofu init` after a provider change),
    /// the next check re-enqueues a fetch. Without this, a user who
    /// added a new provider mid-session would never see its schema
    /// load for the rest of the server's lifetime.
    pub fetched_schema_dirs:
        dashmap::DashMap<std::path::PathBuf, std::time::SystemTime>,

    /// Set to `true` during `initialize` when the client advertises
    /// support for pull-based diagnostics
    /// (`capabilities.textDocument.diagnostic`). When `true` the
    /// server skips push-based `publishDiagnostics` for open
    /// buffers — otherwise nvim (and any other client that stores
    /// both channels separately) ends up with duplicate
    /// diagnostic entries. Default `false` preserves push-only
    /// behaviour for clients that don't do pull.
    pub client_supports_pull_diagnostics:
        std::sync::atomic::AtomicBool,

    /// True when the client advertised support for
    /// `workspace/diagnostic/refresh` at `initialize`
    /// (`capabilities.workspace.diagnostic.refresh_support`). When
    /// `true` the server can nudge the client to re-pull diagnostics
    /// after an async background scan added new cross-file symbols
    /// that could invalidate previous per-file results. Default
    /// `false` so clients that don't advertise it don't get spurious
    /// requests.
    pub client_supports_diagnostic_refresh:
        std::sync::atomic::AtomicBool,

    /// URIs currently open in the client (received `didOpen`, no
    /// matching `didClose` yet). Used to distinguish "client will
    /// pull this" (open) from "client will only see this via push"
    /// (unopened workspace files surfaced by bulk scan).
    pub open_docs: dashmap::DashSet<Url>,
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

    /// True when the server should *skip* push-based
    /// `publishDiagnostics` for `uri` and rely on the client
    /// pulling via `textDocument/diagnostic` instead. Applies
    /// only to URIs currently open in the client — unopened
    /// workspace files are still pushed so workspace-wide views
    /// (`:Trouble workspace_diagnostics`) populate.
    pub fn should_skip_push_diagnostics(&self, uri: &Url) -> bool {
        self.client_supports_pull_diagnostics
            .load(std::sync::atomic::Ordering::Relaxed)
            && self.open_docs.contains(uri)
    }

    /// Mark a URI as open in the client.
    pub fn mark_open(&self, uri: Url) {
        self.open_docs.insert(uri);
    }

    /// Unmark a URI on `didClose`.
    pub fn mark_closed(&self, uri: &Url) {
        self.open_docs.remove(uri);
    }

    /// Is this URI currently open in any client buffer? Used by
    /// cross-file diagnostic refresh to push fresh state only to
    /// buffers the user can actually see.
    pub fn is_open(&self, uri: &Url) -> bool {
        self.open_docs.contains(uri)
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

    /// Insert (or replace) a document and rebuild its indexes.
    pub fn upsert_document(&self, doc: DocumentState) {
        let uri = doc.uri.clone();
        self.remove_from_indexes(&uri);
        self.add_to_indexes(&doc);
        self.documents.insert(uri, doc);
    }

    /// Re-analyse an existing document in place and refresh its indexes.
    pub fn reparse_document(&self, uri: &Url) {
        self.remove_from_indexes(uri);
        if let Some(mut doc) = self.documents.get_mut(uri) {
            doc.reparse();
            self.add_to_indexes(&doc);
        }
    }

    /// Remove a document from the store and from indexes.
    pub fn remove_document(&self, uri: &Url) -> Option<DocumentState> {
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
        self.definitions_by_name
            .retain(|_, v| !v.is_empty());
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

    let references = doc.references.iter().map(|r| reference_key(&r.kind)).collect();
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
        store.upsert_document(DocumentState::new(
            u.clone(),
            r#"variable "region" {}"#,
            1,
        ));
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
    fn reparse_refreshes_indexes() {
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        store.upsert_document(DocumentState::new(u.clone(), r#"variable "old" {}"#, 1));
        assert!(
            store
                .definitions_by_name
                .contains_key(&SymbolKey::new(SymbolKind::Variable, "old"))
        );

        if let Some(mut doc) = store.documents.get_mut(&u) {
            doc.rope = ropey::Rope::from_str(r#"variable "new" {}"#);
        }
        store.reparse_document(&u);

        assert!(
            !store
                .definitions_by_name
                .contains_key(&SymbolKey::new(SymbolKind::Variable, "old"))
        );
        assert!(
            store
                .definitions_by_name
                .contains_key(&SymbolKey::new(SymbolKind::Variable, "new"))
        );
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

    // --- should_skip_push_diagnostics invariants --------------------
    //
    // These pin the contract the indexer relies on to avoid
    // duplicate diagnostics (push + pull showing the same entry
    // twice in nvim's two separate namespaces). The rule:
    //
    //   push is skipped ⇔ pull is advertised AND the URI is open
    //
    // Every change to `should_skip_push_diagnostics` must keep
    // the four cases below passing.

    #[test]
    fn skip_push_when_pull_advertised_and_buffer_open() {
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        store.set_client_supports_pull_diagnostics(true);
        store.mark_open(u.clone());
        assert!(
            store.should_skip_push_diagnostics(&u),
            "open + pull must skip push (otherwise it duplicates \
             against the pull namespace)"
        );
    }

    #[test]
    fn do_not_skip_push_for_closed_buffer_even_under_pull() {
        // Workspace-wide views (`:Trouble workspace_diagnostics`)
        // need pushes for files the user hasn't opened, because
        // pull only targets open buffers.
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        store.set_client_supports_pull_diagnostics(true);
        // Don't mark open.
        assert!(!store.should_skip_push_diagnostics(&u));
    }

    #[test]
    fn do_not_skip_push_for_open_buffer_without_pull() {
        // Push-only clients need push for their open buffers too.
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        store.mark_open(u.clone());
        // Pull not advertised.
        assert!(!store.should_skip_push_diagnostics(&u));
    }

    #[test]
    fn do_not_skip_push_for_closed_push_only_client() {
        let store = StateStore::new();
        let u = uri("file:///a.tf");
        assert!(!store.should_skip_push_diagnostics(&u));
    }
}
