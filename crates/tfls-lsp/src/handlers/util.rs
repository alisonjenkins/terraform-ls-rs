//! Small shared helpers for LSP handlers.

use std::path::{Path, PathBuf};

use lsp_types::Url;
use serde::Deserialize;

/// Filesystem parent directory of a `file://` URI. Returns `None` for
/// URIs that can't be mapped to a path (e.g. exotic or non-file
/// schemes) so callers can degrade gracefully.
pub(crate) fn parent_dir(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()?.parent().map(|p| p.to_path_buf())
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
pub(crate) fn resolve_module_source(
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
}
