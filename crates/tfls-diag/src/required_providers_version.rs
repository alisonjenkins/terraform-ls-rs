//! `terraform_required_providers` — flag any
//! `required_providers { NAME = { … } }` entry that doesn't carry a
//! `version` key. Pinning provider versions avoids surprise
//! breakages when a new major is published.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn required_providers_version_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
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
                let has_version = obj.iter().any(|(k, _v)| match k {
                    hcl_edit::expr::ObjectKey::Ident(id) => id.as_str() == "version",
                    hcl_edit::expr::ObjectKey::Expression(Expression::Variable(v)) => {
                        v.value().as_str() == "version"
                    }
                    hcl_edit::expr::ObjectKey::Expression(Expression::String(s)) => {
                        s.value().as_str() == "version"
                    }
                    _ => false,
                });
                if has_version {
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
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        required_providers_version_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_entry_without_version() {
        let d = diags(
            r#"terraform {
                required_providers {
                    aws = { source = "hashicorp/aws" }
                }
            }"#,
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("`aws`"), "got: {}", d[0].message);
        assert!(d[0].message.contains("version"), "got: {}", d[0].message);
    }

    #[test]
    fn silent_when_version_set() {
        let d = diags(
            r#"terraform {
                required_providers {
                    aws = { source = "hashicorp/aws", version = "~> 5.0" }
                }
            }"#,
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_only_missing_entries_in_mixed_block() {
        let d = diags(
            r#"terraform {
                required_providers {
                    aws    = { source = "hashicorp/aws", version = "~> 5.0" }
                    random = { source = "hashicorp/random" }
                }
            }"#,
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("`random`"), "got: {}", d[0].message);
    }
}
