//! Recursive filesystem walker that yields `.tf` and `.tf.json` files
//! under a workspace root, skipping well-known ignored directories.

use std::path::{Path, PathBuf};

use crate::error::WalkerError;

const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".terraform",
    ".terragrunt-cache",
    ".direnv",
    "node_modules",
    "target",
];

/// Is `name` a directory we should not descend into?
pub fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.contains(&name)
}

/// Walk `root` recursively and return all Terraform source files.
/// Errors when reading a directory are bubbled up with the path that
/// failed.
pub fn discover_terraform_files(root: &Path) -> Result<Vec<PathBuf>, WalkerError> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|source| WalkerError::DirectoryRead {
            path: dir.display().to_string(),
            source,
        })?;

        for entry in entries {
            let entry = entry.map_err(|source| WalkerError::DirectoryRead {
                path: dir.display().to_string(),
                source,
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| WalkerError::DirectoryRead {
                path: path.display().to_string(),
                source,
            })?;

            if file_type.is_dir() {
                let name_is_ignored = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(is_ignored_dir)
                    .unwrap_or(false);
                if !name_is_ignored {
                    stack.push(path);
                }
            } else if file_type.is_file() && is_terraform_file(&path) {
                out.push(path);
            }
        }
    }

    out.sort();
    Ok(out)
}

/// Non-recursive sibling of [`discover_terraform_files`]. Lists `.tf` and
/// `.tf.json` files in `dir` without descending into subdirectories. Used
/// when the editor opens a file and we need to index the enclosing module
/// (a single directory) without pulling in nested modules.
pub fn discover_terraform_files_in_dir(dir: &Path) -> Result<Vec<PathBuf>, WalkerError> {
    let entries = std::fs::read_dir(dir).map_err(|source| WalkerError::DirectoryRead {
        path: dir.display().to_string(),
        source,
    })?;

    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| WalkerError::DirectoryRead {
            path: dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| WalkerError::DirectoryRead {
            path: path.display().to_string(),
            source,
        })?;
        if file_type.is_file() && is_terraform_file(&path) {
            out.push(path);
        }
    }

    out.sort();
    Ok(out)
}

/// Does this path look like a Terraform/OpenTofu source file we should
/// index?
///
/// Covers:
/// - `.tf`, `.tf.json` — Terraform config
/// - `.tofu`, `.tofu.json` — OpenTofu config
/// - `.tftest.hcl`, `.tofutest.hcl` — Terraform/OpenTofu test files
///
/// Bare `.hcl` is intentionally excluded: Packer, Nomad, Consul and other
/// HashiCorp tools also use it, so a naive match produces false positives.
pub fn is_terraform_file(path: &Path) -> bool {
    let name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => n,
        None => return false,
    };
    name.ends_with(".tf")
        || name.ends_with(".tf.json")
        || name.ends_with(".tofu")
        || name.ends_with(".tofu.json")
        || name.ends_with(".tftest.hcl")
        || name.ends_with(".tofutest.hcl")
}

/// Does this path look like a Terraform variable-values file?
///
/// Covers `terraform.tfvars`, `terraform.tfvars.json`, `*.auto.tfvars`,
/// `*.auto.tfvars.json`, and any `*.tfvars` / `*.tfvars.json` Terraform
/// would accept via `-var-file`. We index these for type-inference
/// only — the values they assign let us infer the shape of variables
/// that lack a `default`. They do NOT participate in regular
/// diagnostics: a tfvars file is not a module file.
pub fn is_tfvars_file(path: &Path) -> bool {
    let name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => n,
        None => return false,
    };
    name.ends_with(".tfvars") || name.ends_with(".tfvars.json")
}

/// Non-recursive: list every `*.tfvars` / `*.tfvars.json` in `dir`.
pub fn discover_tfvars_files_in_dir(dir: &Path) -> Result<Vec<PathBuf>, WalkerError> {
    let entries = std::fs::read_dir(dir).map_err(|source| WalkerError::DirectoryRead {
        path: dir.display().to_string(),
        source,
    })?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| WalkerError::DirectoryRead {
            path: dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| WalkerError::DirectoryRead {
            path: path.display().to_string(),
            source,
        })?;
        if file_type.is_file() && is_tfvars_file(&path) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Recursive: every `*.tfvars` / `*.tfvars.json` under `root`. Honours
/// the same `is_ignored_dir` pruning as the regular walker.
pub fn discover_tfvars_files(root: &Path) -> Result<Vec<PathBuf>, WalkerError> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|source| WalkerError::DirectoryRead {
            path: dir.display().to_string(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| WalkerError::DirectoryRead {
                path: dir.display().to_string(),
                source,
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| WalkerError::DirectoryRead {
                path: path.display().to_string(),
                source,
            })?;
            if file_type.is_dir() {
                let name_is_ignored = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(is_ignored_dir)
                    .unwrap_or(false);
                if !name_is_ignored {
                    stack.push(path);
                }
            } else if file_type.is_file() && is_tfvars_file(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tfls-walker-{suffix}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create tmpdir");
        dir
    }

    #[test]
    fn finds_tf_files() {
        let dir = tmp_dir("finds_tf");
        fs::write(dir.join("main.tf"), "").unwrap();
        fs::write(dir.join("vars.tf.json"), "{}").unwrap();
        fs::write(dir.join("README.md"), "").unwrap();

        let files = discover_terraform_files(&dir).expect("walk");
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"main.tf".to_string()));
        assert!(names.contains(&"vars.tf.json".to_string()));
        assert!(!names.contains(&"README.md".to_string()));

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recurses_into_subdirectories() {
        let dir = tmp_dir("recurses");
        let sub = dir.join("modules").join("network");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("main.tf"), "").unwrap();

        let files = discover_terraform_files(&dir).expect("walk");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("modules/network/main.tf"));

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn skips_ignored_dirs() {
        let dir = tmp_dir("skips");
        let hidden = dir.join(".terraform");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("cached.tf"), "").unwrap();
        fs::write(dir.join("main.tf"), "").unwrap();

        let files = discover_terraform_files(&dir).expect("walk");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("main.tf"));

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_root_errors_with_path() {
        let err = discover_terraform_files(Path::new("/nonexistent-tfls-walker-path"));
        match err {
            Err(WalkerError::DirectoryRead { path, source: _ }) => {
                assert!(path.contains("nonexistent"));
            }
            other => panic!("expected DirectoryRead error, got {other:?}"),
        }
    }

    #[test]
    fn is_ignored_dir_covers_terraform() {
        assert!(is_ignored_dir(".terraform"));
        assert!(is_ignored_dir(".git"));
        assert!(!is_ignored_dir("modules"));
    }
}
