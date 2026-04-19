//! `terraform_required_providers` — flag any
//! `required_providers { NAME = { … } }` entry that doesn't carry
//! a `version` key *and* doesn't have one declared in any sibling
//! `required_providers` block elsewhere in the module.
//!
//! The module-wide aggregation matters because Terraform merges
//! `required_providers` across files. A clean setup often puts the
//! `source` in one file and `version` in another; a per-file check
//! would flag both as missing.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::module_graph::ModuleGraphLookup;

pub fn required_providers_version_diagnostics(
    body: &Body,
    rope: &Rope,
    lookup: &dyn ModuleGraphLookup,
) -> Vec<Diagnostic> {
    let with_version = lookup.providers_with_version_set();
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(tf_block) = structure.as_block() else {
            continue;
        };
        if tf_block.ident.as_str() != "terraform" {
            continue;
        }
        for inner in tf_block.body.iter() {
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
                let provider_local_name = attr.key.as_str();
                let Expression::Object(obj) = &attr.value else {
                    continue;
                };
                let has_local_version = obj.iter().any(|(k, _v)| match k {
                    hcl_edit::expr::ObjectKey::Ident(id) => id.as_str() == "version",
                    hcl_edit::expr::ObjectKey::Expression(Expression::Variable(v)) => {
                        v.value().as_str() == "version"
                    }
                    hcl_edit::expr::ObjectKey::Expression(Expression::String(s)) => {
                        s.value().as_str() == "version"
                    }
                    _ => false,
                });
                if has_local_version {
                    continue;
                }
                // Module-level check: is this provider versioned
                // anywhere in the same module?
                if with_version.contains(provider_local_name) {
                    continue;
                }
                let span = attr.span().unwrap_or(0..0);
                let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
                out.push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!(
                        "provider `{provider_local_name}` should declare a `version` constraint"
                    ),
                    ..Default::default()
                });
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tfls_parser::parse_source;

    struct Lookup {
        versioned: HashSet<String>,
    }
    impl ModuleGraphLookup for Lookup {
        fn variable_is_referenced(&self, _: &str) -> bool {
            true
        }
        fn local_is_referenced(&self, _: &str) -> bool {
            true
        }
        fn data_source_is_referenced(&self, _: &str, _: &str) -> bool {
            true
        }
        fn used_provider_locals(&self) -> HashSet<String> {
            HashSet::new()
        }
        fn present_files(&self) -> HashSet<String> {
            HashSet::new()
        }
        fn is_root_module(&self) -> bool {
            true
        }
        fn module_has_required_version(&self) -> bool {
            true
        }
        fn is_primary_terraform_doc(&self) -> bool {
            true
        }
        fn providers_with_version_set(&self) -> HashSet<String> {
            self.versioned.clone()
        }
    }

    fn diags(src: &str, versioned: &[&str]) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        required_providers_version_diagnostics(
            &body,
            &rope,
            &Lookup {
                versioned: versioned.iter().map(|s| s.to_string()).collect(),
            },
        )
    }

    #[test]
    fn flags_entry_without_version_when_module_also_lacks_it() {
        let d = diags(
            r#"terraform {
                required_providers {
                    aws = { source = "hashicorp/aws" }
                }
            }"#,
            &[],
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("`aws`"));
    }

    #[test]
    fn silent_when_version_set_in_same_file() {
        let d = diags(
            r#"terraform {
                required_providers {
                    aws = { source = "hashicorp/aws", version = "~> 5.0" }
                }
            }"#,
            &[],
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_when_version_declared_elsewhere_in_module() {
        // `source` here, `version` in a sibling file — module-level
        // aggregation should suppress the warning.
        let d = diags(
            r#"terraform {
                required_providers {
                    aws = { source = "hashicorp/aws" }
                }
            }"#,
            &["aws"],
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_only_entries_whose_provider_is_unversioned_module_wide() {
        let d = diags(
            r#"terraform {
                required_providers {
                    aws    = { source = "hashicorp/aws" }
                    random = { source = "hashicorp/random" }
                }
            }"#,
            &["aws"],
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("`random`"));
    }
}
