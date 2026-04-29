//! Generic scaffolding for "this Terraform construct is
//! superseded — flag it" diagnostics.
//!
//! Three live consumers (`null_resource`, `template_file`,
//! `template_dir`) all share the same shape: walk top-level
//! `<block_kind> "<label>" "X" { ... }` blocks, emit a WARNING
//! on the label range, suppress when the module's
//! `required_version` excludes the floor at which the
//! replacement landed. The deprecation modules now thin-wrap
//! `DeprecationRule` instead of repeating the walk + version
//! gate three times.
//!
//! Each rule is a plain `const` carrying the small set of
//! parameters that differ between deprecations. Adding a new
//! deprecation that fits this shape is a config entry +
//! one-line wrapper, not a fresh body walker.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::{Body, BlockLabel};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

/// Static description of one "X is deprecated, prefer Y" rule.
#[derive(Debug, Clone, Copy)]
pub struct DeprecationRule {
    /// HCL block kind to match — typically `"resource"` or
    /// `"data"`.
    pub block_kind: &'static str,
    /// First block label whose presence triggers the
    /// diagnostic (e.g. `"null_resource"`, `"template_file"`).
    pub label: &'static str,
    /// Lowest Terraform version that ships the replacement
    /// (e.g. `"1.4.0"` for `terraform_data`, `"0.12.0"` for
    /// `templatefile()`). Constraints whose minimum admitted
    /// version is below this floor suppress the rule.
    pub threshold: &'static str,
    /// User-visible message — should name the replacement and
    /// (when applicable) gesture at the matching code action.
    pub message: &'static str,
}

/// Body-only diagnostic emit. Computes the per-body version
/// gate from the body's own `required_version` (if any). Multi-
/// file modules should prefer [`diagnostics_for_module`] —
/// `required_version` typically lives in `versions.tf`, not the
/// file being scanned.
pub fn diagnostics(rule: &DeprecationRule, body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    diagnostics_for_module(rule, body, rope, body_supports(rule, body))
}

/// Module-aware emit. Caller supplies the precomputed
/// `supports` flag (true = replacement available, fire) so a
/// constraint declared in a sibling can correctly gate this
/// body.
pub fn diagnostics_for_module(
    rule: &DeprecationRule,
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
        if block.ident.as_str() != rule.block_kind {
            continue;
        }
        let Some(label) = block.labels.first() else {
            continue;
        };
        if label_str(label) != Some(rule.label) {
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
            message: rule.message.to_string(),
            ..Default::default()
        });
    }
    out
}

/// True when `constraint` admits a Terraform version at or
/// above the rule's threshold floor (i.e. the replacement
/// exists in every version the user might run). Empty / loose
/// constraints fall through to `true` — we can't suppress on
/// absence of evidence.
pub fn supports(rule: &DeprecationRule, constraint: &str) -> bool {
    let parsed = tfls_core::version_constraint::parse(constraint);
    if parsed.constraints.is_empty() {
        return true;
    }
    let Some(min) = tfls_core::version_constraint::min_admitted_version(&parsed.constraints)
    else {
        return false;
    };
    tfls_core::version_constraint::version_at_least(min, rule.threshold)
}

/// Extract the literal `required_version = "..."` string from
/// the top-level `terraform { }` block, if present. Empty
/// fragments / non-string forms are ignored.
pub fn extract_required_version(body: &Body) -> Option<String> {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
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

fn body_supports(rule: &DeprecationRule, body: &Body) -> bool {
    let Some(constraint) = extract_required_version(body) else {
        return true;
    };
    supports(rule, &constraint)
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

    const TEST_RULE: DeprecationRule = DeprecationRule {
        block_kind: "resource",
        label: "null_resource",
        threshold: "1.4.0",
        message: "test message",
    };

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        diagnostics(&TEST_RULE, &body, &rope)
    }

    #[test]
    fn fires_when_unconstrained() {
        let d = diags("resource \"null_resource\" \"x\" {}\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].message, "test message");
    }

    #[test]
    fn ignores_unrelated_blocks() {
        let d = diags("resource \"aws_instance\" \"x\" { ami = \"a\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn supports_predicate_min_threshold() {
        assert!(supports(&TEST_RULE, ">= 1.4"));
        assert!(supports(&TEST_RULE, "~> 1.5"));
        assert!(!supports(&TEST_RULE, ">= 1.0"));
        assert!(!supports(&TEST_RULE, "= 1.3.5"));
        assert!(!supports(&TEST_RULE, "< 1.3"));
    }

    #[test]
    fn for_module_overrides_body_gate() {
        let src = concat!(
            "terraform { required_version = \"< 1.3\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        // `supports = true` overrides the local `< 1.3` body gate.
        let d = diagnostics_for_module(&TEST_RULE, &body, &rope, true);
        assert_eq!(d.len(), 1);
        let d = diagnostics_for_module(&TEST_RULE, &body, &rope, false);
        assert!(d.is_empty());
    }

    #[test]
    fn extract_required_version_reads_string_form() {
        let src = "terraform { required_version = \">= 1.4\" }\n";
        let body = parse_source(src).body.expect("parses");
        assert_eq!(extract_required_version(&body), Some(">= 1.4".into()));
    }

    #[test]
    fn extract_required_version_returns_none_when_absent() {
        let body = parse_source("resource \"x\" \"y\" {}\n").body.expect("parses");
        assert!(extract_required_version(&body).is_none());
    }
}
