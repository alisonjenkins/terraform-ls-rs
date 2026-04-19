//! `terraform_deprecated_lookup` — flag the two-argument form of
//! `lookup(map, key)`. The third `default` argument was made
//! required in Terraform 0.12's ruleset; the two-arg form is the
//! 0.11 carry-over that tflint warns about.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::expr_walk::for_each_expression;

pub fn deprecated_lookup_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for_each_expression(body, |expr| {
        let Expression::FuncCall(call) = expr else {
            return;
        };
        if !call.name.namespace.is_empty() {
            return;
        }
        if call.name.name.as_str() != "lookup" {
            return;
        }
        if call.args.iter().count() != 2 {
            return;
        }
        let span = call.span().unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: "two-argument `lookup()` is deprecated; pass a default as the third argument"
                .to_string(),
            ..Default::default()
        });
    });
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
        deprecated_lookup_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_two_arg_lookup() {
        let d = diags(r#"output "x" { value = lookup(var.m, "key") }"#);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("deprecated"), "got: {}", d[0].message);
    }

    #[test]
    fn silent_for_three_arg_lookup() {
        let d = diags(r#"output "x" { value = lookup(var.m, "key", "default") }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_non_lookup_calls() {
        let d = diags(r#"output "x" { value = tomap(var.m) }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_lookup_nested_in_other_expression() {
        let d = diags(r#"output "x" { value = concat([lookup(var.m, "k")], []) }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }
}
