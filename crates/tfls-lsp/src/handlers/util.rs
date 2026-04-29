//! Small shared helpers for LSP handlers.

use std::path::{Path, PathBuf};

use lsp_types::{Location, Url};
use serde::Deserialize;
use tfls_core::{SymbolKind, SymbolLocation};
use tfls_state::StateStore;

/// Filesystem parent directory of a `file://` URI. Returns `None` for
/// URIs that can't be mapped to a path (e.g. exotic or non-file
/// schemes) so callers can degrade gracefully.
pub(crate) fn parent_dir(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()?.parent().map(|p| p.to_path_buf())
}

/// True when the active module's `required_version` admits any
/// 1.4+ Terraform — the floor at which `terraform_data` exists.
/// Walks every `.tf` doc in the module dir (constraints typically
/// live in `versions.tf`, not the file the user is editing) and
/// AND-joins their `required_version` strings before deciding.
///
/// Returns `true` when the module declares no constraint (we
/// can't suppress on absence of evidence).
pub(crate) fn module_supports_terraform_data(state: &StateStore, primary_uri: &Url) -> bool {
    module_constraint_admits_at_least(state, primary_uri, tfls_diag::supports_terraform_data)
}

/// True when the active module's `required_version` admits any
/// 0.12+ Terraform — the floor at which `templatefile()` exists.
pub(crate) fn module_supports_templatefile(state: &StateStore, primary_uri: &Url) -> bool {
    module_constraint_admits_at_least(state, primary_uri, tfls_diag::supports_templatefile)
}

/// Helper: aggregate the module's `required_version` fragments
/// and feed them to a per-feature gate predicate. Walks every
/// `.tf` doc in the active module dir.
fn module_constraint_admits_at_least(
    state: &StateStore,
    primary_uri: &Url,
    gate: fn(&str) -> bool,
) -> bool {
    let Some(target_dir) = parent_dir(primary_uri) else {
        return true;
    };
    let mut fragments: Vec<String> = Vec::new();
    for entry in state.documents.iter() {
        let uri = entry.key();
        let Ok(path) = uri.to_file_path() else { continue };
        if path.parent() != Some(&target_dir) {
            continue;
        }
        let doc = entry.value();
        let Some(body) = doc.parsed.body.as_ref() else {
            continue;
        };
        if let Some(s) = tfls_diag::extract_required_version(body) {
            fragments.push(s);
        }
    }
    if fragments.is_empty() {
        return true;
    }
    gate(&fragments.join(", "))
}

/// Resolve a `module "<label>" { source = "<source>" }` reference to a
/// concrete, already-on-disk directory we can index. Handles:
///
/// - **Local** paths (`./foo`, `../foo`, `/abs/foo`, `foo/bar`) —
///   joined against `parent_dir`, then canonicalised. We accept any
///   non-empty source as local when it resolves to an existing
///   directory (matching Terraform's own heuristic).
/// - **Remote** sources (registry, git, HTTP, S3, …) — walk up from
///   `parent_dir` looking for `.terraform/modules/modules.json`
///   (written by `terraform init` / `tofu init`). If an entry's `Key`
///   matches `module_label`, its `Dir` is joined against the manifest's
///   containing directory and returned.
///
/// Returns `None` when no matching directory exists on disk.
pub fn resolve_module_source(
    parent_dir: &Path,
    module_label: &str,
    source: &str,
) -> Option<PathBuf> {
    // 1. Local path first — try joining against the consumer's dir.
    let candidate = parent_dir.join(source);
    if let Ok(canon) = candidate.canonicalize() {
        if canon.is_dir() {
            return Some(canon);
        }
    }

    // 2. Lockfile: walk up, looking for .terraform/modules/modules.json.
    let mut current: Option<&Path> = Some(parent_dir);
    while let Some(dir) = current {
        let manifest = dir.join(".terraform").join("modules").join("modules.json");
        if manifest.is_file() {
            if let Ok(content) = std::fs::read_to_string(&manifest) {
                if let Ok(parsed) = serde_json::from_str::<ModulesManifest>(&content) {
                    for entry in parsed.modules {
                        if entry.key == module_label {
                            let resolved = dir.join(&entry.dir);
                            if let Ok(canon) = resolved.canonicalize() {
                                if canon.is_dir() {
                                    return Some(canon);
                                }
                            }
                        }
                    }
                }
            }
            // Found a manifest but no matching key — stop searching.
            return None;
        }
        current = dir.parent();
    }

    None
}

/// True when the given symbol location sits directly inside
/// `dir`. "Inside" means "its file's parent directory equals
/// `dir`" — a Terraform module is a single directory, so this is
/// the per-module-scope check for "is this symbol in scope from a
/// reference in `dir`". Child-module and unrelated-stack
/// locations return `false`.
pub(crate) fn location_in_dir(loc: &SymbolLocation, dir: &Path) -> bool {
    parent_dir(&loc.uri).as_deref() == Some(dir)
}

/// Look up a declared symbol (variable or output) inside a child
/// module whose source files have been indexed under `child_dir`.
/// Returns the LSP `Location` of the declaring block, suitable for a
/// `textDocument/definition` response.
///
/// Only `SymbolKind::Variable` and `SymbolKind::Output` are
/// meaningful here — the helper is built for "navigate from a module
/// input key / output consumer into the child module's declaration".
/// Any other kind yields `None`.
///
/// The child module's symbols must already live in
/// [`StateStore::documents`] — the workspace indexer populates these
/// recursively via `enqueue_child_module_scans`, so by the time a
/// user triggers goto-definition the tables are ready. We do not
/// trigger on-demand parsing here.
/// Find the `source` value for `module "<module_label>" {}` declared
/// in any `.tf` file whose parent directory matches `module_dir`.
///
/// `module_sources` is populated per-file at parse time, so a
/// reference like `module.foo.bar` in `a.tf` can't find the source of
/// `module "foo"` declared in peer `b.tf` by consulting the
/// referencing document alone. Real Terraform stacks almost always
/// split module calls out into their own file (`k3s_cluster.tf`) and
/// reference them from many peers (`cloudflare.tf`, `route53.tf`, …),
/// so the intra-document lookup misses the common case.
///
/// Iterating the directory's peer documents recovers the mapping at
/// O(files-in-dir) — bounded by module size, not workspace size.
pub fn module_source_in_dir(
    state: &StateStore,
    module_dir: &Path,
    module_label: &str,
) -> Option<String> {
    for entry in state.documents.iter() {
        let Ok(doc_path) = entry.key().to_file_path() else {
            continue;
        };
        if doc_path.parent() != Some(module_dir) {
            continue;
        }
        if let Some(source) = entry.value().symbols.module_sources.get(module_label) {
            return Some(source.clone());
        }
    }
    None
}

pub(crate) fn lookup_child_module_symbol(
    state: &StateStore,
    child_dir: &Path,
    kind: SymbolKind,
    name: &str,
) -> Option<Location> {
    for entry in state.documents.iter() {
        let Ok(doc_path) = entry.key().to_file_path() else {
            continue;
        };
        if doc_path.parent() != Some(child_dir) {
            continue;
        }
        let table = &entry.value().symbols;
        let sym = match kind {
            SymbolKind::Variable => table.variables.get(name),
            SymbolKind::Output => table.outputs.get(name),
            _ => None,
        };
        if let Some(s) = sym {
            return Some(s.location.to_lsp_location());
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct ModulesManifest {
    #[serde(rename = "Modules", default)]
    modules: Vec<ModulesManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct ModulesManifestEntry {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Dir", default)]
    dir: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn resolves_local_relative_path() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("mod");
        std::fs::create_dir(&child).unwrap();
        let got = resolve_module_source(temp.path(), "whatever", "./mod").unwrap();
        assert_eq!(got, child.canonicalize().unwrap());
    }

    #[test]
    fn resolves_lockfile_entry_for_remote_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let cached = root.join("modules").join("web");
        std::fs::create_dir_all(&cached).unwrap();
        std::fs::create_dir_all(root.join(".terraform").join("modules")).unwrap();
        std::fs::write(
            root.join(".terraform").join("modules").join("modules.json"),
            r#"{"Modules":[{"Key":"web","Source":"x","Dir":"modules/web"}]}"#,
        )
        .unwrap();
        let got = resolve_module_source(root, "web", "hashicorp/example/aws").unwrap();
        assert_eq!(got, cached.canonicalize().unwrap());
    }

    #[test]
    fn returns_none_when_nothing_matches() {
        let temp = tempfile::tempdir().unwrap();
        assert!(resolve_module_source(temp.path(), "web", "hashicorp/x/aws").is_none());
    }

    // --- lookup_child_module_symbol ----------------------------------

    use tfls_state::{DocumentState, StateStore};

    /// Build a URI for a file sitting directly inside `dir`.
    fn uri_in(dir: &Path, name: &str) -> Url {
        Url::from_file_path(dir.join(name)).unwrap()
    }

    #[test]
    fn lookup_child_module_variable_hit() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().canonicalize().unwrap();
        let store = StateStore::new();
        let u = uri_in(&child, "variables.tf");
        store.upsert_document(DocumentState::new(
            u.clone(),
            r#"variable "region" { type = string }"#,
            1,
        ));
        let got = lookup_child_module_symbol(
            &store,
            &child,
            SymbolKind::Variable,
            "region",
        );
        let got = got.expect("variable should resolve");
        assert_eq!(got.uri, u);
    }

    #[test]
    fn lookup_child_module_output_hit() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().canonicalize().unwrap();
        let store = StateStore::new();
        let u = uri_in(&child, "outputs.tf");
        store.upsert_document(DocumentState::new(
            u.clone(),
            r#"output "subnet_id" { value = "" }"#,
            1,
        ));
        let got = lookup_child_module_symbol(
            &store,
            &child,
            SymbolKind::Output,
            "subnet_id",
        );
        assert!(got.is_some(), "output should resolve");
    }

    #[test]
    fn lookup_child_module_miss_on_unknown_name() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().canonicalize().unwrap();
        let store = StateStore::new();
        let u = uri_in(&child, "variables.tf");
        store.upsert_document(DocumentState::new(
            u,
            r#"variable "region" {}"#,
            1,
        ));
        assert!(
            lookup_child_module_symbol(&store, &child, SymbolKind::Variable, "nope")
                .is_none()
        );
    }

    #[test]
    fn lookup_child_module_ignores_docs_outside_dir() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let child = child.canonicalize().unwrap();
        let sibling = temp.path().join("sibling");
        std::fs::create_dir(&sibling).unwrap();
        let sibling = sibling.canonicalize().unwrap();

        let store = StateStore::new();
        // Declaration lives in `sibling`, not `child` — lookup must
        // treat the `child` directory as authoritative.
        store.upsert_document(DocumentState::new(
            uri_in(&sibling, "variables.tf"),
            r#"variable "region" {}"#,
            1,
        ));
        assert!(
            lookup_child_module_symbol(&store, &child, SymbolKind::Variable, "region")
                .is_none()
        );
    }
}
