//! `terraform_empty_list_equality` — flag `x == []` and `x != []`.
//! Terraform's `==` does structural comparison, and `[]` typed as
//! tuple(()) makes this always-false in most cases. The idiomatic
//! replacement is `length(x) == 0` / `length(x) > 0`.

use hcl_edit::expr::{BinaryOperator, Expression};
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::expr_walk::for_each_expression;

pub fn empty_list_equality_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for_each_expression(body, |expr| {
        let Expression::BinaryOp(op) = expr else {
            return;
        };
        let op_kind = match op.operator.value() {
            BinaryOperator::Eq => "==",
            BinaryOperator::NotEq => "!=",
            _ => return,
        };
        let lhs_empty = is_empty_array(&op.lhs_expr);
        let rhs_empty = is_empty_array(&op.rhs_expr);
        if !lhs_empty && !rhs_empty {
            return;
        }
        let span = op.span().unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        // `x == []` is always FALSE (use `length(x) == 0`); `x != []` is
        // always TRUE (use `length(x) > 0`). The replacement and the
        // truth value both differ by operator — `length(x) >= 0` would be
        // vacuously always-true, not equivalent to `x != []`.
        let message = match op_kind {
            "==" => "comparing with `== []` is always false; use `length(x) == 0` instead",
            _ => "comparing with `!= []` is always true; use `length(x) > 0` instead",
        }
        .to_string();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message,
            ..Default::default()
        });
    });
    out
}

fn is_empty_array(expr: &Expression) -> bool {
    matches!(expr, Expression::Array(a) if a.iter().next().is_none())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        empty_list_equality_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_equality_with_empty_list() {
        let d = diags(r#"output "x" { value = var.ids == [] }"#);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("length(x)"), "got: {}", d[0].message);
    }

    #[test]
    fn flags_inequality_with_empty_list_on_rhs() {
        let d = diags(r#"output "x" { value = var.ids != [] }"#);
        assert_eq!(d.len(), 1);
        // `!=` is always TRUE and the idiom is `length(x) > 0` — not the
        // `>= 0` / "always false" the rule previously emitted.
        assert!(
            d[0].message.contains("always true"),
            "got: {}",
            d[0].message
        );
        assert!(
            d[0].message.contains("length(x) > 0"),
            "got: {}",
            d[0].message
        );
    }

    #[test]
    fn equality_message_says_always_false_and_eq_zero() {
        let d = diags(r#"output "x" { value = var.ids == [] }"#);
        assert!(
            d[0].message.contains("always false"),
            "got: {}",
            d[0].message
        );
        assert!(
            d[0].message.contains("length(x) == 0"),
            "got: {}",
            d[0].message
        );
    }

    #[test]
    fn flags_empty_list_on_lhs() {
        let d = diags(r#"output "x" { value = [] == var.ids }"#);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn silent_for_non_empty_list_comparison() {
        let d = diags(r#"output "x" { value = var.ids == ["a"] }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_length_pattern() {
        let d = diags(r#"output "x" { value = length(var.ids) == 0 }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }
}
