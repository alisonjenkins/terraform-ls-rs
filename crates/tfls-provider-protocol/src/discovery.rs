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
        format!("{}/{}/{}", self.registry_host, self.namespace, self.name)
    }
}

/// Reduce a list of discovered provider binaries to one per
/// `(registry_host, namespace, name)` tuple, picking the highest
/// version. Terraform's own lock resolver may leave multiple
/// versions of the same provider cached under `.terraform/providers/`
/// — e.g. `hashicorp/aws/{5.94.1, 6.0.0, 6.18.0}/` all co-exist when
/// the lockfile was upgraded without pruning. We want to spawn the
/// gRPC binary + install a schema exactly once per provider, so
/// dedupe before fetch. Falls back to string-compare if a version
/// string isn't valid semver.
pub fn dedupe_providers_keep_highest(mut bins: Vec<ProviderBinary>) -> Vec<ProviderBinary> {
    // Sort in reverse (newest first) so `.dedup_by` keeps the first
    // occurrence of each (host, ns, name) triple — which will be the
    // highest version.
    bins.sort_by(|a, b| {
        let ka = (&a.registry_host, &a.namespace, &a.name);
        let kb = (&b.registry_host, &b.namespace, &b.name);
        ka.cmp(&kb)
            .then_with(|| version_cmp(&b.version, &a.version))
    });
    bins.dedup_by(|a, b| {
        a.registry_host == b.registry_host && a.namespace == b.namespace && a.name == b.name
    });
    bins
}

/// Like [`dedupe_providers_keep_highest`] but consults a
/// `(host, namespace, name) → pinned_version` map to pick the
/// LOCK-PINNED version instead of the highest-on-disk one. Falls
/// back to "highest" when no pin exists for a given provider.
///
/// Why: `tofu init` doesn't always reap superseded provider
/// binaries from `.terraform/providers/`. After a downgrade —
/// e.g. constraint loosened then re-pinned to an older version —
/// both the old AND new binaries sit on disk. Picking the
/// highest gives the user a schema that doesn't match what
/// `terraform plan` will actually run, which surfaces as
/// "unknown attribute" diagnostics that don't fire (the schema
/// includes attrs the binary on disk doesn't expose) and worse
/// false-positives where the schema permits something the
/// binary will reject. The `.terraform.lock.hcl` is the
/// authoritative source — every `init` writes it — so use that.
pub fn dedupe_providers_using_pins(
    mut bins: Vec<ProviderBinary>,
    pins: &std::collections::HashMap<(String, String, String), String>,
) -> Vec<ProviderBinary> {
    // Sort so:
    //  1. Pin-matches sort FIRST within each (host, ns, name) group.
    //  2. Within non-pin-match (or no pin), highest version FIRST.
    // Then `dedup_by` keeps the first occurrence per provider.
    // Canonicalise the host before pin lookup so registry mirrors
    // (`registry.opentofu.org` ↔ `registry.terraform.io`) match
    // each other. Same logic that `ProviderAddress::parse` applies
    // on the lock-file side.
    let canon = |h: &str| {
        match h {
            "registry.opentofu.org" | "registry.terraform.io" => "registry.terraform.io",
            other => other,
        }
        .to_string()
    };
    bins.sort_by(|a, b| {
        let ka = (&a.registry_host, &a.namespace, &a.name);
        let kb = (&b.registry_host, &b.namespace, &b.name);
        ka.cmp(&kb).then_with(|| {
            let pin_a = pins
                .get(&(canon(&a.registry_host), a.namespace.clone(), a.name.clone()))
                .map(|v| v.as_str() == a.version)
                .unwrap_or(false);
            let pin_b = pins
                .get(&(canon(&b.registry_host), b.namespace.clone(), b.name.clone()))
                .map(|v| v.as_str() == b.version)
                .unwrap_or(false);
            // Pin matches before non-matches (true sorts after
            // false by default → swap with reverse).
            pin_b
                .cmp(&pin_a)
                .then_with(|| version_cmp(&b.version, &a.version))
        })
    });
    bins.dedup_by(|a, b| {
        a.registry_host == b.registry_host && a.namespace == b.namespace && a.name == b.name
    });
    bins
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(va), Ok(vb)) => va.cmp(&vb),
        // One or both aren't semver — fall back to lexicographic.
        // Not perfect for odd tags, but safer than panicking; real
        // provider versions are ~always semver.
        _ => a.cmp(b),
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
                    let Some(binary) = find_provider_binary(&arch_path, name) else {
                        continue;
                    };
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

/// Locate a provider binary inside a `<version>/<os>_<arch>/` directory.
///
/// Naming conventions seen in the wild:
///   * `terraform-provider-<name>` (plain — produced by Terraform's own
///     registry tar.gz layout and by `hashicorp/*` providers)
///   * `terraform-provider-<name>_v<version>` (OpenTofu-packaged binaries
///     — cloudflare, github, jose, …)
///   * `terraform-provider-<name>_<version>` (no `v` prefix — e.g. b2)
///
/// Prefer the plain name when it exists, otherwise fall back to any
/// file starting with `terraform-provider-<name>_`. Falling back blindly
/// on the prefix alone would pick up unrelated names like
/// `terraform-provider-awscc` when looking for `aws`, so require a `_`
/// separator.
fn find_provider_binary(arch_path: &Path, name: &str) -> Option<PathBuf> {
    let plain = arch_path.join(format!("terraform-provider-{name}"));
    if plain.is_file() {
        return Some(plain);
    }
    let versioned_prefix = format!("terraform-provider-{name}_");
    let entries = std::fs::read_dir(arch_path).ok()?;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname_str) = fname.to_str() else {
            continue;
        };
        if fname_str.starts_with(&versioned_prefix)
            && entry.file_type().ok().is_some_and(|t| t.is_file())
        {
            return Some(entry.path());
        }
    }
    None
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
        let p =
            std::env::temp_dir().join(format!("tfls-pp-discovery-{}-{nanos}", std::process::id()));
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

    /// Regression: OpenTofu packages many providers as
    /// `terraform-provider-<name>_v<version>` (e.g. cloudflare, github,
    /// jose). Discovery must accept the versioned suffix; otherwise
    /// those providers silently never get their schema loaded and
    /// resource-body completion returns empty for every resource of
    /// that type.
    #[test]
    fn finds_provider_with_versioned_suffix() {
        let dir = tmp();
        let os_arch = current_os_arch_dirname();
        let leaf = dir
            .join("providers")
            .join("registry.opentofu.org")
            .join("cloudflare")
            .join("cloudflare")
            .join("5.18.0")
            .join(&os_arch);
        fs::create_dir_all(&leaf).unwrap();
        let bin = leaf.join("terraform-provider-cloudflare_v5.18.0");
        fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let found = discover_providers(&dir).unwrap();
        assert_eq!(found.len(), 1, "discovered: {found:?}");
        let p = &found[0];
        assert_eq!(p.name, "cloudflare");
        assert_eq!(
            p.full_address(),
            "registry.opentofu.org/cloudflare/cloudflare"
        );
        assert!(
            p.binary.file_name().and_then(|s| s.to_str())
                == Some("terraform-provider-cloudflare_v5.18.0")
        );

        fs::remove_dir_all(dir).ok();
    }

    /// Regression: some providers (b2) use an `_<version>` suffix
    /// without the leading `v`.
    #[test]
    fn finds_provider_with_bare_version_suffix() {
        let dir = tmp();
        let os_arch = current_os_arch_dirname();
        let leaf = dir
            .join("providers")
            .join("registry.opentofu.org")
            .join("backblaze")
            .join("b2")
            .join("0.12.1")
            .join(&os_arch);
        fs::create_dir_all(&leaf).unwrap();
        let bin = leaf.join("terraform-provider-b2_0.12.1");
        fs::write(&bin, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let found = discover_providers(&dir).unwrap();
        assert_eq!(found.len(), 1, "discovered: {found:?}");
        assert_eq!(found[0].name, "b2");

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn dedupe_keeps_highest_semver_version() {
        let mk = |ns: &str, name: &str, ver: &str| ProviderBinary {
            binary: std::path::PathBuf::from("/dev/null"),
            registry_host: "registry.opentofu.org".to_string(),
            namespace: ns.to_string(),
            name: name.to_string(),
            version: ver.to_string(),
        };
        let input = vec![
            mk("hashicorp", "aws", "5.94.1"),
            mk("hashicorp", "aws", "6.18.0"),
            mk("hashicorp", "aws", "6.0.0"),
            mk("hashicorp", "random", "3.8.1"),
            mk("cloudflare", "cloudflare", "5.18.0"),
        ];
        let out = dedupe_providers_keep_highest(input);
        assert_eq!(out.len(), 3, "dedupes aws triple: {out:?}");
        let aws = out.iter().find(|b| b.name == "aws").expect("aws retained");
        assert_eq!(aws.version, "6.18.0", "keeps highest: {aws:?}");
    }

    #[test]
    fn dedupe_with_pins_picks_pinned_over_highest() {
        // Both 4.50.0 and 4.71.0 of azurerm are on disk (e.g.
        // user downgraded, tofu init didn't reap the old binary).
        // Lock pins 4.50.0 — that's what `terraform plan` will run,
        // so that's the schema we should fetch from. Without the
        // pin lookup, we'd take 4.71.0 (highest) and the user's
        // diagnostics would track a binary they're not using.
        let mk = |ns: &str, name: &str, ver: &str| ProviderBinary {
            binary: std::path::PathBuf::from("/dev/null"),
            registry_host: "registry.terraform.io".to_string(),
            namespace: ns.to_string(),
            name: name.to_string(),
            version: ver.to_string(),
        };
        let input = vec![
            mk("hashicorp", "azurerm", "4.50.0"),
            mk("hashicorp", "azurerm", "4.71.0"),
        ];
        let mut pins = std::collections::HashMap::new();
        pins.insert(
            (
                "registry.terraform.io".to_string(),
                "hashicorp".to_string(),
                "azurerm".to_string(),
            ),
            "4.50.0".to_string(),
        );
        let out = dedupe_providers_using_pins(input, &pins);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].version, "4.50.0",
            "lock pin wins over higher: {out:?}"
        );
    }

    #[test]
    fn dedupe_with_pins_falls_back_to_highest_when_no_pin() {
        // Provider not listed in the pin map → behaviour matches
        // dedupe_providers_keep_highest.
        let mk = |ns: &str, name: &str, ver: &str| ProviderBinary {
            binary: std::path::PathBuf::from("/dev/null"),
            registry_host: "registry.terraform.io".to_string(),
            namespace: ns.to_string(),
            name: name.to_string(),
            version: ver.to_string(),
        };
        let input = vec![
            mk("hashicorp", "aws", "6.0.0"),
            mk("hashicorp", "aws", "6.43.0"),
        ];
        let pins = std::collections::HashMap::new();
        let out = dedupe_providers_using_pins(input, &pins);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].version, "6.43.0", "no pin → highest: {out:?}");
    }

    #[test]
    fn dedupe_with_pins_falls_back_to_highest_when_pin_missing_on_disk() {
        // Lock pins a version we don't have on disk (e.g. binary
        // not yet downloaded). Pick the highest available rather
        // than dropping the provider entirely.
        let mk = |ns: &str, name: &str, ver: &str| ProviderBinary {
            binary: std::path::PathBuf::from("/dev/null"),
            registry_host: "registry.terraform.io".to_string(),
            namespace: ns.to_string(),
            name: name.to_string(),
            version: ver.to_string(),
        };
        let input = vec![
            mk("hashicorp", "azurerm", "4.50.0"),
            mk("hashicorp", "azurerm", "4.71.0"),
        ];
        let mut pins = std::collections::HashMap::new();
        pins.insert(
            (
                "registry.terraform.io".to_string(),
                "hashicorp".to_string(),
                "azurerm".to_string(),
            ),
            "4.99.0".to_string(),
        );
        let out = dedupe_providers_using_pins(input, &pins);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].version, "4.71.0", "missing pin → fall back: {out:?}");
    }

    #[test]
    fn dedupe_preserves_different_providers() {
        let mk = |ns: &str, name: &str, ver: &str| ProviderBinary {
            binary: std::path::PathBuf::from("/dev/null"),
            registry_host: "registry.opentofu.org".to_string(),
            namespace: ns.to_string(),
            name: name.to_string(),
            version: ver.to_string(),
        };
        let input = vec![
            mk("hashicorp", "aws", "6.18.0"),
            mk("hashicorp", "random", "3.8.1"),
            mk("cloudflare", "cloudflare", "5.18.0"),
        ];
        let out = dedupe_providers_keep_highest(input);
        assert_eq!(out.len(), 3, "no dedupe needed: {out:?}");
    }

    #[test]
    fn dedupe_with_non_semver_falls_back_to_string_order() {
        let mk = |ver: &str| ProviderBinary {
            binary: std::path::PathBuf::from("/dev/null"),
            registry_host: "registry.opentofu.org".to_string(),
            namespace: "x".to_string(),
            name: "y".to_string(),
            version: ver.to_string(),
        };
        // "v2" and "v10" aren't valid semver; string order puts
        // "v10" < "v2" lexicographically. Keep the string-highest.
        let out = dedupe_providers_keep_highest(vec![mk("v2"), mk("v10")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].version, "v2");
    }

    /// `awscc` must not be picked up when we were looking for `aws` —
    /// the `_` separator in the fallback prefix guards against that.
    #[test]
    fn does_not_confuse_sibling_provider_names() {
        let dir = tmp();
        let os_arch = current_os_arch_dirname();
        let leaf = dir
            .join("providers")
            .join("registry.opentofu.org")
            .join("hashicorp")
            .join("aws")
            .join("6.0.0")
            .join(&os_arch);
        fs::create_dir_all(&leaf).unwrap();
        // Intentionally wrong name present in the directory.
        let wrong = leaf.join("terraform-provider-awscc");
        fs::write(&wrong, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&wrong, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let found = discover_providers(&dir).unwrap();
        assert!(
            found.is_empty(),
            "should not match awscc when name is aws: {found:?}"
        );

        fs::remove_dir_all(dir).ok();
    }
}
