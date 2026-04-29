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
    deprecated_null_resource_diagnostics_for_module(body, rope, terraform_data_supported(body))
}

/// Module-aware variant. Caller supplies the precomputed
/// `supports_terraform_data` decision aggregated across every
/// sibling `.tf` in the module (so a `required_version` declared
/// in `versions.tf` correctly gates `null_resource` blocks in
/// `main.tf`). The body-only gate in
/// [`deprecated_null_resource_diagnostics`] cannot see siblings.
pub fn deprecated_null_resource_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    supports: bool,
) -> Vec<Diagnostic> {
    if !supports {
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

/// `terraform_data` was added in Terraform 1.4. Suppress when
/// the constraint admits any pre-1.4 version — i.e. when the
/// constraint's MIN admitted version is below 1.4.0, or when
/// the constraint sets no lower bound at all (e.g. `< 1.4` or
/// `!= 1.5`, both of which permit ancient Terraform versions).
///
/// `constraint` is the AND-joined `required_version` string —
/// callers in multi-file modules should join every sibling's
/// constraint with `, ` before passing in (HCL constraint AND
/// syntax).
pub fn supports_terraform_data(constraint: &str) -> bool {
    let parsed = tfls_core::version_constraint::parse(constraint);
    if parsed.constraints.is_empty() {
        return true;
    }
    let Some(min) = tfls_core::version_constraint::min_admitted_version(&parsed.constraints)
    else {
        return false;
    };
    tfls_core::version_constraint::version_at_least(min, "1.4.0")
}

/// Extract the literal `required_version = "..."` string out of
/// the top-level `terraform { }` block, if present. Empty
/// fragments / non-string forms are ignored.
pub fn extract_required_version(body: &Body) -> Option<String> {
    required_version_string(body)
}

fn terraform_data_supported(body: &Body) -> bool {
    let Some(constraint) = required_version_string(body) else {
        return true;
    };
    supports_terraform_data(&constraint)
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

    #[test]
    fn suppressed_when_exact_pin_below_1_4() {
        // `= 1.3.5` — exactly one admitted version, predates
        // terraform_data. Probe-set approaches that only test
        // major.minor.0 floors miss this.
        let src = concat!(
            "terraform { required_version = \"= 1.3.5\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_when_exact_pin_at_1_4() {
        let src = concat!(
            "terraform { required_version = \"= 1.4.0\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn suppressed_when_pre_0_11_pin() {
        // Tofu/Terraform pre-0.11 era. The user explicitly
        // asked us to support these — projects on 0.10.x exist.
        let src = concat!(
            "terraform { required_version = \"< 0.11\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn suppressed_when_upper_bound_below_1_4() {
        // `<= 1.3.99` admits 0.x → 1.3.99, all pre-terraform_data.
        let src = concat!(
            "terraform { required_version = \"<= 1.3.99\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn module_aware_helper_fires_when_supports_overridden() {
        // `_for_module` lets the LSP layer override the
        // body-only gate using an aggregated module-wide flag.
        // Even when the file declares a constraint that admits
        // pre-1.4, supports=true (e.g. another sibling pinned
        // >= 1.4) makes the diagnostic fire.
        let src = concat!(
            "terraform { required_version = \"< 1.3\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = deprecated_null_resource_diagnostics_for_module(&body, &rope, true);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn module_aware_helper_suppresses_when_supports_false() {
        let src = "resource \"null_resource\" \"x\" {}\n";
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = deprecated_null_resource_diagnostics_for_module(&body, &rope, false);
        assert!(d.is_empty());
    }
}
