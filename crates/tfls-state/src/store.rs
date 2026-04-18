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
    /// Directories we have already enumerated for `.tf` files. Each Terraform
    /// module is a single directory, so when a file from a not-yet-seen
    /// directory is opened in the editor we enqueue a scan of that dir so
    /// sibling files' symbols become resolvable (fixes false-positive
    /// undefined-reference diagnostics across unrelated workspace roots).
    pub scanned_dirs: dashmap::DashSet<std::path::PathBuf>,

    /// Terraform init-root directories (containing a `.terraform/providers/`
    /// subtree) we have already enqueued a schema fetch for. Dedupes the
    /// cross-module FetchSchemas enqueues triggered from did_open.
    pub fetched_schema_dirs: dashmap::DashSet<std::path::PathBuf>,
}

impl StateStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a batch of function signatures, replacing any previous set.
    pub fn install_functions(&self, schema: FunctionsSchema) {
        self.functions.clear();
        for (name, sig) in schema.function_signatures {
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
}
