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

/// Walk `module_dir`'s subtree and return every `*.tfvars` /
/// `*.tfvars.json` file that should be attributed to `module_dir`'s
/// variables — i.e. files whose nearest `.tf`/`.tf.json`-bearing
/// ancestor is `module_dir`.
///
/// Use case: a common Terraform layout puts environment-specific
/// var-files under sibling subdirs that contain no `.tf` of their own
/// (`params/nonprod/params.tfvars`, `envs/prod/terraform.tfvars`,
/// `vars/staging.auto.tfvars`). Terraform applies these to the root
/// module via `-var-file=`. For LSP type inference we mirror that
/// semantic: any tfvars in a subtree that isn't itself a module
/// (no `.tf` files in the dir) feeds the closest ancestor module.
///
/// Crucially, sibling MODULE dirs are skipped — a tfvars file that
/// happens to live next to `modules/foo/main.tf` belongs to `foo`,
/// not the parent calling `foo`. We detect this by stopping descent
/// (and skipping tfvars discovery) at any non-root subdir that
/// contains `.tf` / `.tf.json` files of its own.
pub fn discover_tfvars_attributable_to(
    module_dir: &Path,
) -> Result<Vec<PathBuf>, WalkerError> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, bool)> = vec![(module_dir.to_path_buf(), true)];
    while let Some((dir, is_root)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|source| WalkerError::DirectoryRead {
            path: dir.display().to_string(),
            source,
        })?;
        let mut subdirs: Vec<PathBuf> = Vec::new();
        let mut tfvars: Vec<PathBuf> = Vec::new();
        let mut has_tf = false;
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
                    subdirs.push(path);
                }
            } else if file_type.is_file() {
                if is_terraform_file(&path) {
                    has_tf = true;
                } else if is_tfvars_file(&path) {
                    tfvars.push(path);
                }
            }
        }
        // Include this dir's tfvars and descend further when:
        //  - we're at `module_dir` (always — its own tfvars belong
        //    to it, even if it also has its own `.tf` siblings); OR
        //  - this is a deeper "tfvars-only" holder (no `.tf`).
        // Otherwise the dir IS its own module: stop, leave its
        // tfvars to it.
        let include = is_root || !has_tf;
        if include {
            out.extend(tfvars);
            for sd in subdirs {
                stack.push((sd, false));
            }
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

    #[test]
    fn attributable_includes_module_dir_tfvars() {
        let dir = tmp_dir("attr_root_tfvars");
        fs::write(dir.join("main.tf"), "").unwrap();
        fs::write(dir.join("terraform.tfvars"), "x = 1").unwrap();

        let found = discover_tfvars_attributable_to(&dir).expect("walk");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file_name().unwrap(), "terraform.tfvars");

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn attributable_includes_tfvars_only_subdir() {
        // Common layout: `params/nonprod/params.tfvars` feeds the
        // root module via `-var-file`, no `.tf` next to it.
        let dir = tmp_dir("attr_subdir");
        fs::write(dir.join("main.tf"), "").unwrap();
        fs::create_dir_all(dir.join("params/nonprod")).unwrap();
        fs::create_dir_all(dir.join("params/prod")).unwrap();
        fs::write(dir.join("params/nonprod/params.tfvars"), "envtype = \"nonprod\"").unwrap();
        fs::write(dir.join("params/prod/params.tfvars"), "envtype = \"prod\"").unwrap();

        let found = discover_tfvars_attributable_to(&dir).expect("walk");
        let names: Vec<_> = found
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"params/nonprod/params.tfvars".to_string()), "{names:?}");
        assert!(names.contains(&"params/prod/params.tfvars".to_string()), "{names:?}");

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn attributable_excludes_sibling_module_tfvars() {
        // `modules/foo/foo.tfvars` is foo's own var-file (foo has
        // its own `.tf` files). The PARENT module shouldn't claim it.
        let dir = tmp_dir("attr_sibling_module");
        fs::write(dir.join("main.tf"), "").unwrap();
        fs::create_dir_all(dir.join("modules/foo")).unwrap();
        fs::write(dir.join("modules/foo/main.tf"), "").unwrap();
        fs::write(dir.join("modules/foo/foo.tfvars"), "x = 1").unwrap();

        let found = discover_tfvars_attributable_to(&dir).expect("walk");
        assert!(found.is_empty(), "unexpected: {found:?}");

        // But invoking on `modules/foo` itself should pick it up.
        let nested = discover_tfvars_attributable_to(&dir.join("modules/foo")).expect("walk");
        assert_eq!(nested.len(), 1);

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn attributable_skips_ignored_dirs() {
        let dir = tmp_dir("attr_ignored");
        fs::write(dir.join("main.tf"), "").unwrap();
        fs::create_dir_all(dir.join(".terraform/modules")).unwrap();
        fs::write(dir.join(".terraform/modules/cache.tfvars"), "y = 2").unwrap();

        let found = discover_tfvars_attributable_to(&dir).expect("walk");
        assert!(found.is_empty(), "unexpected: {found:?}");

        fs::remove_dir_all(dir).ok();
    }
}
