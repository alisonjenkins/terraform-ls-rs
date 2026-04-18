//! Walk `.terraform/providers/<host>/<namespace>/<name>/<version>/<os>_<arch>/`
//! directories and yield one provider binary per installed provider.
//!
//! For each provider version, we only pick the `<os>_<arch>` directory
//! matching the current platform (running a Linux binary on macOS or
//! vice versa is guaranteed to fail, so skipping early saves a spawn).

use std::path::{Path, PathBuf};

use crate::ProtocolError;

/// A discovered provider binary alongside its registry coordinates.
#[derive(Debug, Clone)]
pub struct ProviderBinary {
    pub binary: PathBuf,
    pub registry_host: String,
    pub namespace: String,
    pub name: String,
    pub version: String,
}

impl ProviderBinary {
    /// Full terraform-style provider address, e.g.
    /// `registry.terraform.io/hashicorp/aws`. Used as the key in
    /// [`tfls_schema::ProviderSchemas::provider_schemas`].
    pub fn full_address(&self) -> String {
        format!(
            "{}/{}/{}",
            self.registry_host, self.namespace, self.name
        )
    }
}

/// Walk `<terraform_dir>/providers/` (not recursive beyond the usual
/// nested provider layout) and yield one binary per installed provider.
/// Returns an empty list if the directory doesn't exist.
pub fn discover_providers(terraform_dir: &Path) -> Result<Vec<ProviderBinary>, ProtocolError> {
    let providers_root = terraform_dir.join("providers");
    if !providers_root.is_dir() {
        return Ok(Vec::new());
    }

    let target_os_arch = current_os_arch_dirname();
    let mut out = Vec::new();

    for host_entry in read_dir(&providers_root)? {
        let host_path = host_entry.path();
        let Some(registry_host) = file_name_str(&host_path) else {
            continue;
        };
        if !host_path.is_dir() {
            continue;
        }
        for ns_entry in read_dir(&host_path)? {
            let ns_path = ns_entry.path();
            let Some(namespace) = file_name_str(&ns_path) else {
                continue;
            };
            if !ns_path.is_dir() {
                continue;
            }
            for name_entry in read_dir(&ns_path)? {
                let name_path = name_entry.path();
                let Some(name) = file_name_str(&name_path) else {
                    continue;
                };
                if !name_path.is_dir() {
                    continue;
                }
                for version_entry in read_dir(&name_path)? {
                    let version_path = version_entry.path();
                    let Some(version) = file_name_str(&version_path) else {
                        continue;
                    };
                    if !version_path.is_dir() {
                        continue;
                    }

                    let arch_path = version_path.join(&target_os_arch);
                    if !arch_path.is_dir() {
                        continue;
                    }
                    let binary_name = format!("terraform-provider-{name}");
                    let binary = arch_path.join(&binary_name);
                    if !binary.is_file() {
                        continue;
                    }
                    out.push(ProviderBinary {
                        binary,
                        registry_host: registry_host.to_string(),
                        namespace: namespace.to_string(),
                        name: name.to_string(),
                        version: version.to_string(),
                    });
                }
            }
        }
    }

    Ok(out)
}

fn read_dir(path: &Path) -> Result<Vec<std::fs::DirEntry>, ProtocolError> {
    let iter = std::fs::read_dir(path).map_err(ProtocolError::Discovery)?;
    iter.collect::<Result<Vec<_>, _>>()
        .map_err(ProtocolError::Discovery)
}

fn file_name_str(path: &Path) -> Option<&str> {
    path.file_name().and_then(|s| s.to_str())
}

/// Returns `"linux_amd64"` etc. Matches terraform's own directory
/// naming convention.
fn current_os_arch_dirname() -> String {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    format!("{os}_{arch}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "tfls-pp-discovery-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_providers_dir_returns_empty() {
        let dir = tmp();
        let found = discover_providers(&dir).unwrap();
        assert!(found.is_empty());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn finds_a_mock_provider_binary() {
        let dir = tmp();
        let os_arch = current_os_arch_dirname();
        let leaf = dir
            .join("providers")
            .join("registry.opentofu.org")
            .join("hashicorp")
            .join("null")
            .join("3.2.3")
            .join(&os_arch);
        fs::create_dir_all(&leaf).unwrap();
        let bin = leaf.join("terraform-provider-null");
        fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let found = discover_providers(&dir).unwrap();
        assert_eq!(found.len(), 1, "discovered: {found:?}");
        let p = &found[0];
        assert_eq!(p.registry_host, "registry.opentofu.org");
        assert_eq!(p.namespace, "hashicorp");
        assert_eq!(p.name, "null");
        assert_eq!(p.version, "3.2.3");
        assert_eq!(p.full_address(), "registry.opentofu.org/hashicorp/null");

        fs::remove_dir_all(dir).ok();
    }
}
