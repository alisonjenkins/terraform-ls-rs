//! `.terraform.lock.hcl` parsing.
//!
//! `terraform init` writes this file at the module root next to
//! `.terraform/`. It pins the exact provider version selected for
//! each declared `required_providers` entry. The language server
//! consults it to gate diagnostics and code actions on the
//! installed-and-running version, not just the lower bound of the
//! declared constraint.
//!
//! File shape:
//!
//! ```hcl
//! provider "registry.terraform.io/hashicorp/aws" {
//!   version     = "5.50.0"
//!   constraints = "~> 5.0"
//!   hashes      = [ ... ]
//! }
//! ```
//!
//! Only the `version` and `constraints` attributes are read; the
//! `hashes` list is preserved on disk by `terraform init` and is
//! irrelevant to gating decisions.

use std::path::Path;

use hcl_edit::expr::Expression;
use hcl_edit::structure::{Body, BlockLabel};
use rustc_hash::FxHashMap;

use crate::types::ProviderAddress;

/// One `provider "<addr>" { }` entry from a parsed lock file.
#[derive(Debug, Clone)]
pub struct LockFileEntry {
    pub address: ProviderAddress,
    pub version: semver::Version,
    pub constraints: Option<String>,
}

/// Parsed lock file, indexed by canonical [`ProviderAddress`].
#[derive(Debug, Clone, Default)]
pub struct LockFile {
    entries: FxHashMap<ProviderAddress, LockFileEntry>,
}

impl LockFile {
    /// Exact-address lookup. Matches host + namespace + type
    /// strictly. Useful when the caller already has a canonical
    /// address.
    pub fn get(&self, addr: &ProviderAddress) -> Option<&LockFileEntry> {
        self.entries.get(addr).or_else(|| self.find_by_ns_name(&addr.namespace, &addr.r#type))
    }

    /// Host-tolerant lookup by `(namespace, type)`. Matches the
    /// first entry regardless of which registry host the lock
    /// file uses (`registry.terraform.io` vs
    /// `registry.opentofu.org`). Required because the lock file
    /// stores the host the user's CLI fetched the provider from
    /// (tofu writes `registry.opentofu.org`, terraform writes
    /// `registry.terraform.io`), but the LSP-side caller usually
    /// only knows the short-form `<ns>/<name>` from
    /// `required_providers` and defaults the host to
    /// `registry.terraform.io`.
    pub fn find_by_ns_name(&self, namespace: &str, name: &str) -> Option<&LockFileEntry> {
        self.entries
            .iter()
            .find(|(addr, _)| addr.namespace == namespace && addr.r#type == name)
            .map(|(_, e)| e)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ProviderAddress, &LockFileEntry)> {
        self.entries.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Parse a `.terraform.lock.hcl` source string. Entries with
/// malformed addresses or unparseable `version` strings are
/// skipped (logged at `warn`), since the lock file is generated
/// by `terraform init` and a malformed entry is unusual enough
/// to flag but not fatal — gating just falls back to the
/// declared constraint for that provider.
pub fn parse(src: &str) -> LockFile {
    let body = match hcl_edit::parser::parse_body(src) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse .terraform.lock.hcl; treating as empty");
            return LockFile::default();
        }
    };
    parse_body(&body)
}

fn parse_body(body: &Body) -> LockFile {
    let mut entries: FxHashMap<ProviderAddress, LockFileEntry> = FxHashMap::default();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "provider" {
            continue;
        }
        let Some(label) = block.labels.first() else {
            continue;
        };
        let Some(addr_str) = label_str(label) else {
            continue;
        };
        let Ok(address) = ProviderAddress::parse(addr_str) else {
            tracing::warn!(addr = %addr_str, "lock file: skipping unparseable provider address");
            continue;
        };
        let mut version: Option<semver::Version> = None;
        let mut constraints: Option<String> = None;
        for inner in block.body.iter() {
            let Some(attr) = inner.as_attribute() else {
                continue;
            };
            match attr.key.as_str() {
                "version" => {
                    if let Some(s) = read_string_value(&attr.value) {
                        match semver::Version::parse(&s) {
                            Ok(v) => version = Some(v),
                            Err(e) => tracing::warn!(
                                addr = %address,
                                version = %s,
                                error = %e,
                                "lock file: skipping unparseable version"
                            ),
                        }
                    }
                }
                "constraints" => {
                    if let Some(s) = read_string_value(&attr.value) {
                        constraints = Some(s);
                    }
                }
                _ => {}
            }
        }
        let Some(version) = version else {
            continue;
        };
        entries.insert(
            address.clone(),
            LockFileEntry {
                address,
                version,
                constraints,
            },
        );
    }
    LockFile { entries }
}

/// Read the lock file at `<module_dir>/.terraform.lock.hcl`. Returns
/// `None` when the file does not exist or cannot be read; absence is
/// the normal case (workspace not yet `terraform init`-ed).
pub fn read_for_module(module_dir: &Path) -> Option<LockFile> {
    let path = module_dir.join(".terraform.lock.hcl");
    let src = std::fs::read_to_string(&path).ok()?;
    Some(parse(&src))
}

fn label_str(label: &BlockLabel) -> Option<&str> {
    match label {
        BlockLabel::String(s) => Some(s.value().as_str()),
        BlockLabel::Ident(id) => Some(id.as_str()),
    }
}

fn read_string_value(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(s.value().to_string()),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
provider "registry.terraform.io/hashicorp/aws" {
  version     = "5.50.0"
  constraints = "~> 5.0"
  hashes = [
    "h1:abc",
    "zh:def",
  ]
}

provider "registry.terraform.io/hashicorp/random" {
  version = "3.6.2"
  hashes  = []
}
"#;

    #[test]
    fn parses_well_formed_entries() {
        let lf = parse(SAMPLE);
        assert_eq!(lf.len(), 2);
        let aws = lf
            .get(&ProviderAddress::hashicorp("aws"))
            .expect("aws entry");
        assert_eq!(aws.version, semver::Version::new(5, 50, 0));
        assert_eq!(aws.constraints.as_deref(), Some("~> 5.0"));
        let random = lf
            .get(&ProviderAddress::hashicorp("random"))
            .expect("random entry");
        assert_eq!(random.version, semver::Version::new(3, 6, 2));
        assert!(random.constraints.is_none());
    }

    #[test]
    fn short_form_address_lookup_works() {
        // Lock file uses full form; lookups should work with the
        // short-form-derived ProviderAddress as the key. Both forms
        // canonicalise to the same struct.
        let lf = parse(SAMPLE);
        let key = ProviderAddress::parse("hashicorp/aws").expect("short form");
        assert!(lf.get(&key).is_some());
    }

    #[test]
    fn skips_entries_with_unparseable_version() {
        let src = r#"
provider "registry.terraform.io/hashicorp/aws" {
  version = "not-a-version"
}
provider "registry.terraform.io/hashicorp/random" {
  version = "3.0.0"
}
"#;
        let lf = parse(src);
        assert_eq!(lf.len(), 1);
        assert!(lf.get(&ProviderAddress::hashicorp("random")).is_some());
        assert!(lf.get(&ProviderAddress::hashicorp("aws")).is_none());
    }

    #[test]
    fn missing_constraints_attribute_is_ok() {
        let src = r#"
provider "registry.terraform.io/hashicorp/x" {
  version = "1.0.0"
}
"#;
        let lf = parse(src);
        let entry = lf.get(&ProviderAddress::hashicorp("x")).expect("x entry");
        assert!(entry.constraints.is_none());
    }

    #[test]
    fn entries_without_version_are_skipped() {
        let src = r#"
provider "registry.terraform.io/hashicorp/x" {
  constraints = "~> 1.0"
}
"#;
        let lf = parse(src);
        assert_eq!(lf.len(), 0);
    }

    #[test]
    fn malformed_root_returns_empty() {
        // Garbage shouldn't panic, just yield an empty lock file.
        let lf = parse("???not-hcl???");
        assert!(lf.is_empty());
    }

    #[test]
    fn non_provider_blocks_are_ignored() {
        let src = r#"
terraform {
  required_version = "1.0.0"
}
provider "registry.terraform.io/hashicorp/x" {
  version = "1.0.0"
}
"#;
        let lf = parse(src);
        assert_eq!(lf.len(), 1);
    }

    #[test]
    fn read_for_module_returns_none_when_missing() {
        let dir = std::env::temp_dir().join(format!(
            "tfls-lockfile-test-{}",
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_for_module(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_for_module_parses_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "tfls-lockfile-test-read-{}",
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".terraform.lock.hcl"), SAMPLE).unwrap();
        let lf = read_for_module(&dir).expect("present");
        assert_eq!(lf.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
