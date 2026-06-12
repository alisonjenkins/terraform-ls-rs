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
//! - `replace_triggered_by`: a list of managed-resource instance references
//!   only ("Only managed resource instances can be used in
//!   replace_triggered_by"). Index operators may be literals or the
//!   enclosing resource's own `count.index` / `each.key` / `each.value`.
//!   `terraform_data.x.output` is the documented escape hatch for arbitrary
//!   values.
//!
//! `precondition` / `postcondition` accept expressions and are not checked.

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
        // `[count.index]` / `[each.key]` indexes in replace_triggered_by are
        // only legal when the enclosing resource has that meta-argument.
        let mut has_count = false;
        let mut has_for_each = false;
        for entry in block.body.iter() {
            if let Some(attr) = entry.as_attribute() {
                match attr.key.as_str() {
                    "count" => has_count = true,
                    "for_each" => has_for_each = true,
                    _ => {}
                }
            }
        }
        for entry in block.body.iter() {
            let Some(lifecycle) = entry.as_block() else {
                continue;
            };
            if lifecycle.ident.as_str() != "lifecycle" {
                continue;
            }
            check_lifecycle(&lifecycle.body, rope, has_count, has_for_each, &mut out);
        }
    }
    out
}

fn check_lifecycle(
    body: &Body,
    rope: &Rope,
    has_count: bool,
    has_for_each: bool,
    out: &mut Vec<Diagnostic>,
) {
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
            "replace_triggered_by" => {
                if replace_triggered_by_is_legal(&attr.value, has_count, has_for_each) {
                    continue;
                }
                "`replace_triggered_by` only accepts managed resource instance references \
                 (e.g. `aws_instance.web` or `aws_instance.web.id`) — \"Only managed \
                 resource instances can be used in replace_triggered_by\". To trigger on \
                 an arbitrary value, route it through a `terraform_data` resource \
                 (`input = …`) and reference `terraform_data.x.output`."
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

fn replace_triggered_by_is_legal(value: &Expression, has_count: bool, has_for_each: bool) -> bool {
    let Expression::Array(arr) = value else {
        return false;
    };
    arr.iter()
        .all(|el| is_managed_resource_ref(el, has_count, has_for_each))
}

/// A managed-resource instance reference: `<type>.<name>`, optionally with
/// deeper `.attr` segments, and index operators that are literals or the
/// enclosing resource's own `count.index` / `each.key` / `each.value`.
fn is_managed_resource_ref(expr: &Expression, has_count: bool, has_for_each: bool) -> bool {
    let Expression::Traversal(t) = expr else {
        return false;
    };
    let Expression::Variable(head) = &t.expr else {
        return false;
    };
    if RESERVED_ROOTS.contains(&head.as_str()) {
        return false;
    }
    let mut get_attrs = 0usize;
    for op in t.operators.iter() {
        match op.value() {
            TraversalOperator::GetAttr(_) => get_attrs += 1,
            TraversalOperator::Index(idx) => {
                if !replace_index_is_legal(idx, has_count, has_for_each) {
                    return false;
                }
            }
            _ => return false, // splat etc.
        }
    }
    // Need at least `<type>.<name>`.
    get_attrs >= 1
}

fn replace_index_is_legal(idx: &Expression, has_count: bool, has_for_each: bool) -> bool {
    match idx {
        Expression::String(_) | Expression::Number(_) => true,
        Expression::Traversal(t) => {
            let Expression::Variable(head) = &t.expr else {
                return false;
            };
            let attrs: Vec<&str> = t
                .operators
                .iter()
                .filter_map(|op| match op.value() {
                    TraversalOperator::GetAttr(i) => Some(i.as_str()),
                    _ => None,
                })
                .collect();
            match (head.as_str(), attrs.as_slice()) {
                ("count", ["index"]) => has_count,
                ("each", ["key" | "value"]) => has_for_each,
                _ => false,
            }
        }
        _ => false,
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
                TraversalOperator::Index(idx) => {
                    matches!(idx, Expression::String(_) | Expression::Number(_))
                }
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
    fn silent_for_legal_replace_triggered_by() {
        let src = resource_with_lifecycle(
            "    replace_triggered_by = [aws_instance.web, aws_instance.web.id, \
             aws_instance.web[0].id, terraform_data.rev.output]\n    prevent_destroy = true",
        );
        assert!(!flagged(&src), "got: {:?}", diags(&src));
    }

    #[test]
    fn silent_for_count_index_with_count() {
        let src = r#"
resource "aws_s3_bucket" "b" {
  count = 2
  lifecycle {
    replace_triggered_by = [aws_instance.web[count.index]]
  }
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn silent_for_each_key_with_for_each() {
        let src = r#"
resource "aws_s3_bucket" "b" {
  for_each = var.buckets
  lifecycle {
    replace_triggered_by = [aws_instance.web[each.key].id]
  }
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_count_index_without_count() {
        let src =
            resource_with_lifecycle("    replace_triggered_by = [aws_instance.web[count.index]]");
        assert!(flagged(&src));
    }

    #[test]
    fn flags_each_key_without_for_each() {
        let src =
            resource_with_lifecycle("    replace_triggered_by = [aws_instance.web[each.key]]");
        assert!(flagged(&src));
    }

    #[test]
    fn flags_var_in_replace_triggered_by() {
        let src = resource_with_lifecycle("    replace_triggered_by = [var.revision]");
        let d = diags(&src);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("terraform_data"));
    }

    #[test]
    fn flags_local_and_data_in_replace_triggered_by() {
        for entry in ["local.rev", "data.aws_ami.a.id"] {
            let src = resource_with_lifecycle(&format!("    replace_triggered_by = [{entry}]"));
            assert!(flagged(&src), "{entry} should be illegal");
        }
    }

    #[test]
    fn flags_function_and_literal_entries() {
        for entry in ["timestamp()", "\"web\"", "aws_instance.web[*].id"] {
            let src = resource_with_lifecycle(&format!("    replace_triggered_by = [{entry}]"));
            assert!(flagged(&src), "{entry} should be illegal");
        }
    }

    #[test]
    fn flags_non_array_replace_triggered_by() {
        let src = resource_with_lifecycle("    replace_triggered_by = var.list");
        assert!(flagged(&src));
    }

    #[test]
    fn flags_bare_type_only_reference() {
        // A bare identifier is not a `<type>.<name>` instance reference.
        let src = resource_with_lifecycle("    replace_triggered_by = [aws_instance]");
        assert!(flagged(&src));
    }
}
