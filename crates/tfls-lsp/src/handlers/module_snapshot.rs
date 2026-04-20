//! Precomputed per-module data used to short-circuit the module-
//! wide aggregates that every cross-file diagnostic walker would
//! otherwise recompute from scratch. Building this once per bulk
//! scan drops the scan's cross-file cost from O(N²) DashMap reads
//! (one walk per diagnostic call × N diagnostic calls) to O(N).
//!
//! Built from a [`StateStore`] snapshot at a point in time. If the
//! store changes afterwards, callers should rebuild — the diagnostic
//! pipeline already reruns on every edit.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use hcl_edit::expr::{Expression, ObjectKey};
use tfls_state::{StateStore, SymbolKey};
use tfls_core::{SymbolKind};
use tower_lsp::lsp_types::Url;

pub struct ModuleSnapshot {
    pub module_dir: Option<PathBuf>,
    pub has_required_version: bool,
    pub providers_with_version: HashSet<String>,
    pub used_provider_locals: HashSet<String>,
    pub primary_terraform_uri: Option<Url>,
    pub present_files: HashSet<String>,
    pub is_root: bool,
}

impl ModuleSnapshot {
    /// Build all module-wide aggregates in a single pass over
    /// `state.documents`. `module_dir` filters which documents
    /// contribute — only docs whose parent directory matches are
    /// considered part of the module.
    pub fn build(state: &StateStore, module_dir: Option<&Path>) -> Self {
        let mut has_required_version = false;
        let mut providers_with_version: HashSet<String> = HashSet::new();
        let mut used_provider_locals: HashSet<String> = HashSet::new();
        let mut terraform_uri_candidates: Vec<String> = Vec::new();

        for doc in state.documents.iter() {
            if !in_module(doc.key(), module_dir) {
                continue;
            }
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };

            // One pass over the body collects everything we need.
            for structure in body.iter() {
                let Some(block) = structure.as_block() else {
                    continue;
                };
                match block.ident.as_str() {
                    "terraform" => {
                        // required_version
                        if block.body.iter().any(|s| {
                            s.as_attribute()
                                .is_some_and(|a| a.key.as_str() == "required_version")
                        }) {
                            has_required_version = true;
                        }
                        // Track URI for "primary terraform doc" logic.
                        terraform_uri_candidates.push(doc.key().as_str().to_string());
                        // Providers declared in required_providers entries.
                        for inner in block.body.iter() {
                            let Some(rp_block) = inner.as_block() else {
                                continue;
                            };
                            if rp_block.ident.as_str() != "required_providers" {
                                continue;
                            }
                            for entry in rp_block.body.iter() {
                                let Some(attr) = entry.as_attribute() else {
                                    continue;
                                };
                                let name = attr.key.as_str();
                                let Expression::Object(obj) = &attr.value else {
                                    continue;
                                };
                                let has_version = obj.iter().any(|(k, _v)| match k {
                                    ObjectKey::Ident(id) => id.as_str() == "version",
                                    ObjectKey::Expression(Expression::Variable(v)) => {
                                        v.value().as_str() == "version"
                                    }
                                    ObjectKey::Expression(Expression::String(s)) => {
                                        s.value().as_str() == "version"
                                    }
                                    _ => false,
                                });
                                if has_version {
                                    providers_with_version.insert(name.to_string());
                                }
                            }
                        }
                    }
                    "resource" | "data" => {
                        if let Some(label) = block.labels.first() {
                            let type_name = match label {
                                hcl_edit::structure::BlockLabel::String(s) => {
                                    s.value().as_str()
                                }
                                hcl_edit::structure::BlockLabel::Ident(i) => i.as_str(),
                            };
                            if let Some(local) = type_name.split('_').next() {
                                if !local.is_empty() {
                                    used_provider_locals.insert(local.to_string());
                                }
                            }
                        }
                        for attr in block.body.iter().filter_map(|s| s.as_attribute()) {
                            if attr.key.as_str() == "provider" {
                                if let Some(local) = extract_provider_local(&attr.value) {
                                    used_provider_locals.insert(local);
                                }
                            }
                        }
                    }
                    "provider" => {
                        if let Some(label) = block.labels.first() {
                            let name = match label {
                                hcl_edit::structure::BlockLabel::String(s) => {
                                    s.value().as_str().to_string()
                                }
                                hcl_edit::structure::BlockLabel::Ident(i) => {
                                    i.as_str().to_string()
                                }
                            };
                            used_provider_locals.insert(name);
                        }
                    }
                    _ => {}
                }
            }
        }

        terraform_uri_candidates.sort();
        let primary_terraform_uri = terraform_uri_candidates
            .first()
            .and_then(|s| Url::parse(s).ok());

        let present_files = compute_present_files(module_dir);
        let is_root = compute_is_root(state, module_dir);

        Self {
            module_dir: module_dir.map(Path::to_path_buf),
            has_required_version,
            providers_with_version,
            used_provider_locals,
            primary_terraform_uri,
            present_files,
            is_root,
        }
    }

    pub fn variable_is_referenced(&self, state: &StateStore, name: &str) -> bool {
        self.symbol_referenced(state, SymbolKind::Variable, name)
    }

    pub fn local_is_referenced(&self, state: &StateStore, name: &str) -> bool {
        self.symbol_referenced(state, SymbolKind::Local, name)
    }

    pub fn data_source_is_referenced(
        &self,
        state: &StateStore,
        type_name: &str,
        name: &str,
    ) -> bool {
        let key = SymbolKey::resource(SymbolKind::DataSource, type_name, name);
        self.has_ref(state, &key)
    }

    fn symbol_referenced(&self, state: &StateStore, kind: SymbolKind, name: &str) -> bool {
        self.has_ref(state, &SymbolKey::new(kind, name))
    }

    fn has_ref(&self, state: &StateStore, key: &SymbolKey) -> bool {
        let Some(locs) = state.references_by_name.get(key) else {
            return false;
        };
        match &self.module_dir {
            Some(dir) => locs.iter().any(|loc| {
                crate::handlers::util::parent_dir(&loc.uri).as_deref() == Some(dir.as_path())
            }),
            None => !locs.is_empty(),
        }
    }
}

fn in_module(uri: &Url, module_dir: Option<&Path>) -> bool {
    match module_dir {
        Some(dir) => crate::handlers::util::parent_dir(uri).as_deref() == Some(dir),
        None => true,
    }
}

fn extract_provider_local(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Variable(v) => Some(v.value().as_str().to_string()),
        Expression::Traversal(t) => {
            if let Expression::Variable(v) = &t.expr {
                Some(v.value().as_str().to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn compute_present_files(module_dir: Option<&Path>) -> HashSet<String> {
    let Some(dir) = module_dir else {
        return HashSet::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashSet::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .map(|s| s.to_string())
                .filter(|s| s.ends_with(".tf") || s.ends_with(".tf.json"))
        })
        .collect()
}

fn compute_is_root(state: &StateStore, module_dir: Option<&Path>) -> bool {
    let Some(dir) = module_dir else {
        return true;
    };
    for doc in state.documents.iter() {
        let Some(body) = doc.parsed.body.as_ref() else {
            continue;
        };
        let doc_dir = crate::handlers::util::parent_dir(doc.key());
        if doc_dir.as_deref() == Some(dir) {
            continue;
        }
        for structure in body.iter() {
            let Some(block) = structure.as_block() else {
                continue;
            };
            if block.ident.as_str() != "module" {
                continue;
            }
            for attr in block.body.iter().filter_map(|s| s.as_attribute()) {
                if attr.key.as_str() != "source" {
                    continue;
                }
                if let Expression::String(s) = &attr.value {
                    if source_points_at(s.value().as_str(), doc_dir.as_deref(), dir) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn source_points_at(
    source: &str,
    caller_dir: Option<&Path>,
    target: &Path,
) -> bool {
    if !(source.starts_with("./") || source.starts_with("../") || source.starts_with('/')) {
        return false;
    }
    let Some(caller_dir) = caller_dir else {
        return false;
    };
    let resolved = caller_dir.join(source);
    let resolved = match std::fs::canonicalize(&resolved) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let target = match std::fs::canonicalize(target) {
        Ok(p) => p,
        Err(_) => return false,
    };
    resolved == target
}

/// Trait adapter that combines a [`ModuleSnapshot`] with a reference
/// to the underlying [`StateStore`] (for the per-symbol reference
/// lookups that remain cheap on a DashMap) and the current URI (for
/// `is_primary_terraform_doc`).
pub struct CachedModuleLookup<'a> {
    pub snapshot: &'a ModuleSnapshot,
    pub state: &'a StateStore,
    pub current_uri: &'a Url,
}

impl tfls_diag::ModuleGraphLookup for CachedModuleLookup<'_> {
    fn variable_is_referenced(&self, name: &str) -> bool {
        self.snapshot.variable_is_referenced(self.state, name)
    }

    fn local_is_referenced(&self, name: &str) -> bool {
        self.snapshot.local_is_referenced(self.state, name)
    }

    fn data_source_is_referenced(&self, type_name: &str, name: &str) -> bool {
        self.snapshot
            .data_source_is_referenced(self.state, type_name, name)
    }

    fn used_provider_locals(&self) -> HashSet<String> {
        self.snapshot.used_provider_locals.clone()
    }

    fn present_files(&self) -> HashSet<String> {
        self.snapshot.present_files.clone()
    }

    fn is_root_module(&self) -> bool {
        self.snapshot.is_root
    }

    fn module_has_required_version(&self) -> bool {
        self.snapshot.has_required_version
    }

    fn is_primary_terraform_doc(&self) -> bool {
        self.snapshot
            .primary_terraform_uri
            .as_ref()
            .map(|u| u.as_str() == self.current_uri.as_str())
            .unwrap_or(false)
    }

    fn providers_with_version_set(&self) -> HashSet<String> {
        self.snapshot.providers_with_version.clone()
    }
}
