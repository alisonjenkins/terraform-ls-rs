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
    ///
    /// `referenced_dirs` is an OPTIONAL precomputed set of
    /// directories that some module block in the workspace
    /// resolves its `source = "./…"` to. When present, `is_root`
    /// is derived via an O(1) set lookup (`!referenced.contains(
    /// dir)`) instead of re-walking every indexed document and
    /// canonicalising every module source path for every
    /// snapshot. Bulk scans should precompute it once via
    /// [`referenced_dirs_in_workspace`] and pass it to every
    /// `build` call — drops the module-root determination from
    /// O(M · N · canonicalize) to O(M). `None` falls back to the
    /// per-snapshot walk, which is fine for single-file callers
    /// that only build one snapshot.
    pub fn build(
        state: &StateStore,
        module_dir: Option<&Path>,
        referenced_dirs: Option<&HashSet<PathBuf>>,
    ) -> Self {
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
            // Provider-defined function calls (`provider::<local>::<fn>(...)`)
            // count as "used" too — otherwise renaming a provider local
            // and using it only via this 1.8+ syntax would trip
            // unused-required-providers.
            crate::handlers::document::collect_provider_function_locals(
                &doc.rope.to_string(),
                &mut used_provider_locals,
            );
        }

        terraform_uri_candidates.sort();
        let primary_terraform_uri = terraform_uri_candidates
            .first()
            .and_then(|s| Url::parse(s).ok());

        let present_files = compute_present_files(module_dir);
        let is_root = match referenced_dirs {
            // Fast path: the caller has already indexed every
            // module source across the workspace. `is_root` is a
            // plain set membership check — no filesystem syscall,
            // no per-document body walk.
            Some(set) => is_root_via_set(module_dir, set),
            // Slow fallback for callers that build a one-off
            // snapshot (single-file diagnostic paths). Walks the
            // store and canonicalises each local-path source.
            None => compute_is_root(state, module_dir),
        };

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

/// Walk every indexed document in `state` once and collect the
/// set of directories that some `module { source = "./…" }`
/// block resolves to. The result powers [`ModuleSnapshot::build`]'s
/// fast-path `is_root` determination: a module is "root" iff its
/// directory is NOT in this set.
///
/// Canonicalises each local-path source once — not once per
/// target module. For a workspace with K total module blocks
/// across all files, this is K canonicalize calls, regardless
/// of how many target directories we'll eventually build
/// snapshots for. Previously the same K syscalls happened
/// *per target module*, yielding O(M · K) filesystem work during
/// the bulk scan's compute phase.
pub fn referenced_dirs_in_workspace(state: &StateStore) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    for doc in state.documents.iter() {
        let Some(body) = doc.parsed.body.as_ref() else {
            continue;
        };
        let caller_dir = crate::handlers::util::parent_dir(doc.key());
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
                let Expression::String(s) = &attr.value else {
                    continue;
                };
                let source = s.value().as_str();
                if !(source.starts_with("./")
                    || source.starts_with("../")
                    || source.starts_with('/'))
                {
                    continue;
                }
                let Some(dir) = caller_dir.as_deref() else {
                    continue;
                };
                let resolved = dir.join(source);
                if let Ok(canon) = std::fs::canonicalize(&resolved) {
                    out.insert(canon);
                }
            }
        }
    }
    out
}

/// Fast-path `is_root` for a module whose directory is being
/// checked against a precomputed set of referenced directories.
/// Returns `true` iff `module_dir` is NOT the target of some
/// workspace module call.
fn is_root_via_set(module_dir: Option<&Path>, referenced: &HashSet<PathBuf>) -> bool {
    let Some(dir) = module_dir else {
        return true;
    };
    // Membership check needs the same canonical form the
    // precompute wrote. Canonicalise once per call — fine here,
    // the hot path (inside `build`) calls us once per module
    // snapshot, not per document.
    match std::fs::canonicalize(dir) {
        Ok(canon) => !referenced.contains(&canon),
        // Can't canonicalise → treat as root. Nonexistent /
        // permission-denied dirs are never "consumed by a module
        // call" so the generous default matches
        // `compute_is_root`'s behaviour on the slow path.
        Err(_) => true,
    }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! Unit tests for the fast-path `is_root` determination.
    //!
    //! The invariant the tests pin: the precomputed
    //! `referenced_dirs` set plus `ModuleSnapshot::build`'s
    //! fast path produce the SAME `is_root` answer the slow
    //! `compute_is_root` would have produced. Regressing on
    //! that would silently suppress `unused_declarations`
    //! diagnostics (marking root modules as non-root) or spam
    //! them on child modules (marking children as root) —
    //! neither observable via an integration smoke.
    //!
    //! Tests use real filesystems via `tempfile::tempdir`
    //! because `canonicalize` hits the filesystem — mocking
    //! would drift from reality.
    use super::*;
    use std::fs;
    use tfls_state::{DocumentState, StateStore};

    fn make_store_with(files: &[(PathBuf, &str)]) -> StateStore {
        let store = StateStore::new();
        for (path, src) in files {
            fs::write(path, src).unwrap();
            let uri = Url::from_file_path(path).unwrap();
            store.upsert_document(DocumentState::new(uri, src, 1));
        }
        store
    }

    #[test]
    fn referenced_dirs_collects_local_path_module_sources() {
        // Root module references `./modules/net` — precompute
        // must return `{canonical(./modules/net)}`.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let child = root.join("modules").join("net");
        fs::create_dir_all(&child).unwrap();
        let store = make_store_with(&[(
            root.join("main.tf"),
            "module \"net\" { source = \"./modules/net\" }\n",
        )]);

        let referenced = referenced_dirs_in_workspace(&store);
        assert!(
            referenced.contains(&child),
            "precompute missed `./modules/net`: {referenced:?}"
        );
    }

    #[test]
    fn referenced_dirs_skips_registry_sources() {
        // Registry / git / HTTP sources don't resolve to local
        // workspace dirs and must NOT be added to the set —
        // they'd never match a `module_dir` anyway and including
        // them would waste allocations.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let store = make_store_with(&[(
            root.join("main.tf"),
            "module \"vpc\" { source = \"terraform-aws-modules/vpc/aws\" }\n",
        )]);

        let referenced = referenced_dirs_in_workspace(&store);
        assert!(
            referenced.is_empty(),
            "registry source leaked into referenced set: {referenced:?}"
        );
    }

    #[test]
    fn fast_path_is_root_matches_slow_path() {
        // Pin the equivalence between the fast precomputed path
        // and the slow O(N) fallback. If these ever diverge,
        // `unused_declarations` will silently fire/suppress the
        // wrong set of diagnostics.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let child = root.join("modules").join("net");
        fs::create_dir_all(&child).unwrap();
        let store = make_store_with(&[
            (
                root.join("main.tf"),
                "module \"net\" { source = \"./modules/net\" }\n",
            ),
            (
                child.join("variables.tf"),
                "variable \"cidr\" {}\n",
            ),
        ]);

        let referenced = referenced_dirs_in_workspace(&store);

        // Root module (no incoming module references).
        let snap_root_fast = ModuleSnapshot::build(&store, Some(&root), Some(&referenced));
        let snap_root_slow = ModuleSnapshot::build(&store, Some(&root), None);
        assert!(snap_root_fast.is_root);
        assert_eq!(snap_root_fast.is_root, snap_root_slow.is_root);

        // Child module (referenced by root's `module` block).
        let snap_child_fast = ModuleSnapshot::build(&store, Some(&child), Some(&referenced));
        let snap_child_slow = ModuleSnapshot::build(&store, Some(&child), None);
        assert!(!snap_child_fast.is_root);
        assert_eq!(snap_child_fast.is_root, snap_child_slow.is_root);
    }

    #[test]
    fn fast_path_treats_missing_dir_as_root() {
        // `is_root_via_set` can't canonicalise a nonexistent
        // dir — fall back to `true` (generous default, matching
        // `compute_is_root`'s behaviour on errors). Without this
        // the server would misreport live modules as
        // non-root during filesystem flakes.
        let store = StateStore::new();
        let referenced: HashSet<PathBuf> = HashSet::new();
        let missing = std::env::temp_dir().join("tfls-module-snapshot-does-not-exist");
        let _ = fs::remove_dir_all(&missing);

        let snap = ModuleSnapshot::build(&store, Some(&missing), Some(&referenced));
        assert!(snap.is_root);
    }

    #[test]
    fn fast_path_none_referenced_dirs_uses_slow_fallback() {
        // `None` signals "no precompute, do it yourself" — used
        // by the single-file diagnostic path that doesn't batch.
        // Result must match the slow path.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let store = make_store_with(&[(
            root.join("main.tf"),
            "variable \"x\" {}\n",
        )]);

        let snap = ModuleSnapshot::build(&store, Some(&root), None);
        assert!(snap.is_root, "standalone module must be root");
    }
}
