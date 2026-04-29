//! `terraform_deprecated_null_resource` — flag uses of
//! `resource "null_resource" "X"` in projects where the
//! Terraform 1.4+ replacement `terraform_data` is available.
//!
//! Version-aware: suppressed when the module's `terraform { }`
//! block carries a `required_version` constraint that EXCLUDES
//! 1.4.0 (no point nagging the user about a feature their
//! pinned toolchain can't run).
//!
//! Pairs with the `null-resource-to-terraform-data` code
//! action.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::{Body, BlockLabel};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn deprecated_null_resource_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    if !terraform_data_supported(body) {
        return Vec::new();
    }

    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "resource" {
            continue;
        }
        let Some(label) = block.labels.first() else {
            continue;
        };
        if label_str(label) != Some("null_resource") {
            continue;
        }
        let Some(span) = label.span() else { continue };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: "`null_resource` is superseded by the built-in `terraform_data` (Terraform 1.4+) — \
                     use the \"Convert null_resource to terraform_data\" code action."
                .to_string(),
            ..Default::default()
        });
    }
    out
}

/// `terraform_data` was added in Terraform 1.4. The gate's
/// real question is: "could the user end up running a version
/// that DOESN'T have it?" — if so, suppress so we don't nag.
///
/// We answer by probing a battery of representative pre-1.4
/// release versions against the constraint. If ANY pre-1.4
/// version satisfies, the constraint is loose enough that the
/// user might actually be running it, so we suppress.
///
/// Probe set covers the practical version space — the Terraform
/// versioning cadence doesn't leave large gaps, so a handful
/// of representatives is sufficient.
fn terraform_data_supported(body: &Body) -> bool {
    let Some(constraint) = required_version_string(body) else {
        return true;
    };
    let parsed = tfls_core::version_constraint::parse(&constraint);
    if parsed.constraints.is_empty() {
        return true;
    }
    const PRE_1_4_PROBES: &[&str] = &[
        "0.11.0", "0.12.0", "0.13.0", "0.14.0", "0.15.0",
        "1.0.0", "1.1.0", "1.2.0", "1.3.0", "1.3.999",
    ];
    !PRE_1_4_PROBES.iter().any(|v| {
        tfls_core::version_constraint::satisfies_all(&parsed.constraints, v)
    })
}

fn required_version_string(body: &Body) -> Option<String> {
    for structure in body.iter() {
        let block = structure.as_block()?;
        if block.ident.as_str() != "terraform" {
            continue;
        }
        for sub in block.body.iter() {
            let Some(attr) = sub.as_attribute() else {
                continue;
            };
            if attr.key.as_str() != "required_version" {
                continue;
            }
            return match &attr.value {
                Expression::String(s) => Some(s.as_str().to_string()),
                Expression::StringTemplate(t) => {
                    let mut acc = String::new();
                    for element in t.iter() {
                        match element {
                            hcl_edit::template::Element::Literal(lit) => {
                                acc.push_str(lit.as_str());
                            }
                            _ => return None,
                        }
                    }
                    Some(acc)
                }
                _ => None,
            };
        }
    }
    None
}

fn label_str(label: &BlockLabel) -> Option<&str> {
    match label {
        BlockLabel::String(s) => Some(s.value().as_str()),
        BlockLabel::Ident(i) => Some(i.as_str()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        deprecated_null_resource_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_null_resource_when_unconstrained() {
        let d = diags("resource \"null_resource\" \"x\" {}\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("terraform_data"));
    }

    #[test]
    fn ignores_other_resources() {
        let d = diags("resource \"aws_instance\" \"x\" { ami = \"a\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn suppressed_when_required_version_excludes_1_4() {
        let src = concat!(
            "terraform { required_version = \"< 1.3\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_when_required_version_admits_1_4() {
        let src = concat!(
            "terraform { required_version = \">= 1.4\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn fires_when_required_version_pessimistic_admits_1_4() {
        // `~> 1.4` means `>= 1.4.0, < 2.0.0` — admits 1.4+.
        let src = concat!(
            "terraform { required_version = \"~> 1.4\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn fires_when_required_version_pessimistic_min_above_1_4() {
        // `~> 1.5` means `>= 1.5.0, < 2.0.0`. Min admitted
        // version (1.5) is post-1.4, so terraform_data exists
        // for every version the user could be running — fire.
        let src = concat!(
            "terraform { required_version = \"~> 1.5\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn suppressed_when_required_version_admits_pre_1_4() {
        // `>= 1.0` admits 1.0, 1.1, 1.2, 1.3 (no terraform_data).
        let src = concat!(
            "terraform { required_version = \">= 1.0\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn flags_each_block_separately() {
        let src = concat!(
            "resource \"null_resource\" \"a\" {}\n",
            "resource \"null_resource\" \"b\" {}\n",
        );
        assert_eq!(diags(src).len(), 2);
    }
}
