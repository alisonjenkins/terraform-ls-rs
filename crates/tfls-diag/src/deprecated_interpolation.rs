//! `terraform_deprecated_interpolation` — flag whole-string
//! interpolation like `"${var.x}"` where the whole string body is
//! just one interpolation. Since 0.12, `var.x` works directly; the
//! interpolation wrapper is a 0.11 carry-over that hurts readability.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use hcl_edit::template::Element;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::expr_walk::for_each_expression;

pub fn deprecated_interpolation_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for_each_expression(body, |expr| {
        let Expression::StringTemplate(tpl) = expr else {
            return;
        };
        // "Whole-string" interpolation means the template contains
        // exactly one element and that element is an interpolation
        // (not a literal, not a directive). Strings with literal
        // text around `${…}` are legitimate composition.
        let mut iter = tpl.iter();
        let first = iter.next();
        if iter.next().is_some() {
            return;
        }
        let Some(Element::Interpolation(_)) = first else {
            return;
        };
        let span = tpl.span().unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: "interpolation-only expressions are deprecated; use the expression directly"
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
        deprecated_interpolation_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_whole_string_interpolation() {
        let d = diags(r#"output "x" { value = "${var.region}" }"#);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("deprecated"), "got: {}", d[0].message);
    }

    #[test]
    fn silent_for_composed_template() {
        let d = diags(r#"output "x" { value = "region-${var.region}" }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_bare_expression() {
        let d = diags(r#"output "x" { value = var.region }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_multi_interpolation() {
        let d = diags(r#"output "x" { value = "${var.a}${var.b}" }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }
}
