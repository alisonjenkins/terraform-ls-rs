//! `terraform_lifecycle_literal` — flag non-literal values in a resource's
//! `lifecycle` block. Terraform processes `lifecycle` arguments before
//! expression evaluation, so they must be literals; anything else is a hard
//! `terraform validate`-time error:
//!
//! ```text
//! Error: Variables may not be used here.
//! ```
//!
//! Checked:
//! - `prevent_destroy` / `create_before_destroy`: literal booleans only.
//! - `ignore_changes`: the keyword `all`, or a list of static attribute
//!   references (`tags`, `tags["Name"]`, `metadata[0].annotations`). No
//!   variables, no quoted strings, no expressions.
//!
//! `replace_triggered_by` and `precondition` / `postcondition` accept
//! expressions / references and are not checked here.

use hcl_edit::expr::{Expression, TraversalOperator};
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;

use crate::unknown_value::expr_range;

/// Reference roots that mark an `ignore_changes` entry as an expression
/// rather than an attribute path relative to the resource.
const RESERVED_ROOTS: &[&str] = &[
    "var",
    "local",
    "data",
    "module",
    "each",
    "count",
    "terraform",
    "path",
    "self",
];

pub fn lifecycle_literal_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "resource" {
            continue;
        }
        for entry in block.body.iter() {
            let Some(lifecycle) = entry.as_block() else {
                continue;
            };
            if lifecycle.ident.as_str() != "lifecycle" {
                continue;
            }
            check_lifecycle(&lifecycle.body, rope, &mut out);
        }
    }
    out
}

fn check_lifecycle(body: &Body, rope: &Rope, out: &mut Vec<Diagnostic>) {
    for entry in body.iter() {
        let Some(attr) = entry.as_attribute() else {
            continue;
        };
        let key = attr.key.as_str();
        let message = match key {
            "prevent_destroy" | "create_before_destroy" => {
                if matches!(attr.value, Expression::Bool(_)) {
                    continue;
                }
                format!(
                    "`{key}` must be a literal boolean — Terraform processes `lifecycle` \
                     before expression evaluation and rejects anything else with \
                     \"Variables may not be used here\"."
                )
            }
            "ignore_changes" => {
                if ignore_changes_is_literal(&attr.value) {
                    continue;
                }
                "`ignore_changes` must be the keyword `all` or a list of static attribute \
                 references (e.g. `[tags, metadata]`) — Terraform processes `lifecycle` \
                 before expression evaluation and rejects variables, quoted strings, and \
                 expressions here."
                    .to_string()
            }
            _ => continue,
        };
        out.push(Diagnostic {
            range: expr_range(&attr.value, rope),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("terraform-ls-rs".to_string()),
            message,
            ..Default::default()
        });
    }
}

fn ignore_changes_is_literal(value: &Expression) -> bool {
    match value {
        // The `all` keyword parses as a bare identifier.
        Expression::Variable(v) => v.as_str() == "all",
        Expression::Array(arr) => arr.iter().all(is_static_attribute_path),
        _ => false,
    }
}

/// A static attribute path relative to the resource: a bare identifier
/// (`tags`) or a traversal rooted at one (`tags["Name"]`,
/// `metadata[0].annotations`) with only literal index operators.
fn is_static_attribute_path(expr: &Expression) -> bool {
    match expr {
        Expression::Variable(v) => !RESERVED_ROOTS.contains(&v.as_str()),
        Expression::Traversal(t) => {
            let Expression::Variable(head) = &t.expr else {
                return false;
            };
            if RESERVED_ROOTS.contains(&head.as_str()) {
                return false;
            }
            t.operators.iter().all(|op| match op.value() {
                TraversalOperator::GetAttr(_) => true,
                TraversalOperator::Index(idx) => matches!(
                    idx,
                    Expression::String(_) | Expression::Number(_)
                ),
                _ => false,
            })
        }
        _ => false,
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
        lifecycle_literal_diagnostics(&body, &rope)
    }

    fn flagged(src: &str) -> bool {
        !diags(src).is_empty()
    }

    fn resource_with_lifecycle(lifecycle: &str) -> String {
        format!(
            r#"
resource "aws_s3_bucket" "b" {{
  bucket = "x"
  lifecycle {{
{lifecycle}
  }}
}}
"#
        )
    }

    #[test]
    fn silent_for_literal_bools() {
        let src = resource_with_lifecycle(
            "    prevent_destroy       = true\n    create_before_destroy = false",
        );
        assert!(!flagged(&src), "got: {:?}", diags(&src));
    }

    #[test]
    fn flags_var_prevent_destroy() {
        let src = resource_with_lifecycle("    prevent_destroy = var.protect");
        let d = diags(&src);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(d[0].message.contains("prevent_destroy"));
    }

    #[test]
    fn flags_conditional_prevent_destroy() {
        let src =
            resource_with_lifecycle("    prevent_destroy = var.env == \"prod\" ? true : false");
        assert!(flagged(&src));
    }

    #[test]
    fn flags_negated_bool() {
        let src = resource_with_lifecycle("    create_before_destroy = !var.x");
        assert!(flagged(&src));
    }

    #[test]
    fn silent_for_ignore_changes_all() {
        let src = resource_with_lifecycle("    ignore_changes = all");
        assert!(!flagged(&src), "got: {:?}", diags(&src));
    }

    #[test]
    fn silent_for_attribute_paths() {
        let src = resource_with_lifecycle(
            "    ignore_changes = [tags, tags[\"Name\"], metadata[0].annotations]",
        );
        assert!(!flagged(&src), "got: {:?}", diags(&src));
    }

    #[test]
    fn flags_var_entry() {
        let src = resource_with_lifecycle("    ignore_changes = [var.ignored]");
        assert!(flagged(&src));
    }

    #[test]
    fn flags_local_as_whole_list() {
        let src = resource_with_lifecycle("    ignore_changes = local.ignored");
        assert!(flagged(&src));
    }

    #[test]
    fn flags_interpolated_string_entry() {
        let src = resource_with_lifecycle("    ignore_changes = [\"${var.field}\"]");
        assert!(flagged(&src));
    }

    #[test]
    fn flags_dynamic_index() {
        let src = resource_with_lifecycle("    ignore_changes = [tags[var.key]]");
        assert!(flagged(&src));
    }

    #[test]
    fn ignores_lifecycle_on_data_blocks() {
        let src = r#"
data "aws_ami" "a" {
  lifecycle {
    postcondition {
      condition     = self.state == "available"
      error_message = "unavailable"
    }
  }
}
"#;
        assert!(!flagged(src));
    }

    #[test]
    fn ignores_replace_triggered_by_and_conditions() {
        let src = resource_with_lifecycle(
            "    replace_triggered_by = [aws_instance.web.id]\n    prevent_destroy = true",
        );
        assert!(!flagged(&src), "got: {:?}", diags(&src));
    }
}
