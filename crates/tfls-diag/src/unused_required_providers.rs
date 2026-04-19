//! `terraform_unused_required_providers` — flag entries in
//! `terraform { required_providers { ... } }` whose local name isn't
//! used by any `resource`, `data`, or explicit `provider` block in
//! the module.

use std::collections::HashSet;

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::module_graph::ModuleGraphLookup;

pub fn unused_required_providers_diagnostics(
    body: &Body,
    rope: &Rope,
    lookup: &dyn ModuleGraphLookup,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let used: HashSet<String> = lookup.used_provider_locals();
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
                let name = attr.key.as_str();
                if used.contains(name) {
                    continue;
                }
                let span = attr.key.span().unwrap_or(0..0);
                let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
                out.push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!(
                        "provider `{name}` is declared in required_providers but not used"
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
    use tfls_parser::parse_source;

    struct FakeLookup {
        used_providers: HashSet<String>,
    }
    impl ModuleGraphLookup for FakeLookup {
        fn variable_is_referenced(&self, _name: &str) -> bool {
            true
        }
        fn local_is_referenced(&self, _name: &str) -> bool {
            true
        }
        fn data_source_is_referenced(&self, _type_name: &str, _name: &str) -> bool {
            true
        }
        fn used_provider_locals(&self) -> HashSet<String> {
            self.used_providers.clone()
        }
        fn present_files(&self) -> HashSet<String> {
            HashSet::new()
        }
        fn is_root_module(&self) -> bool {
            true
        }
    }

    fn diags(src: &str, used: &[&str]) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let lookup = FakeLookup {
            used_providers: used.iter().map(|s| s.to_string()).collect(),
        };
        unused_required_providers_diagnostics(&body, &rope, &lookup)
    }

    #[test]
    fn flags_unused_provider() {
        let d = diags(
            r#"terraform {
                required_providers {
                    aws    = { source = "hashicorp/aws", version = "~> 5.0" }
                    random = { source = "hashicorp/random", version = "~> 3.0" }
                }
            }"#,
            &["aws"],
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("`random`"), "got: {}", d[0].message);
    }

    #[test]
    fn silent_when_all_used() {
        let d = diags(
            r#"terraform {
                required_providers {
                    aws = { source = "hashicorp/aws", version = "~> 5.0" }
                }
            }"#,
            &["aws"],
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_when_no_required_providers_block() {
        let d = diags(
            r#"terraform { required_version = ">= 1.6" }"#,
            &[],
        );
        assert!(d.is_empty(), "got: {d:?}");
    }
}
