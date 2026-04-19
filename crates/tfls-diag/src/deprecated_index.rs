//! `terraform_deprecated_index` — flag legacy attribute-style
//! indexing on numeric indices (`foo.0`, `foo.1`). Terraform now
//! requires `foo[0]` / `foo[1]`; the old form is a 0.11 carry-over.

use hcl_edit::expr::{Expression, TraversalOperator};
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::expr_walk::for_each_expression;

pub fn deprecated_index_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for_each_expression(body, |expr| {
        if let Expression::Traversal(t) = expr {
            for op in t.operators.iter() {
                if let TraversalOperator::LegacyIndex(idx) = op.value() {
                    let span = op.span().unwrap_or(0..0);
                    let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
                    let n = *idx.value();
                    out.push(Diagnostic {
                        range,
                        severity: Some(DiagnosticSeverity::WARNING),
                        source: Some("terraform-ls-rs".to_string()),
                        message: format!(
                            "legacy attribute-style index `.{n}` is deprecated; use `[{n}]` instead"
                        ),
                        ..Default::default()
                    });
                }
            }
        }
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
        deprecated_index_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_legacy_numeric_index() {
        let d = diags(r#"output "x" { value = aws_vpc.main.0.id }"#);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains(".0"), "got: {}", d[0].message);
        assert!(d[0].message.contains("[0]"), "got: {}", d[0].message);
    }

    #[test]
    fn silent_for_bracket_index() {
        let d = diags(r#"output "x" { value = aws_vpc.main[0].id }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_named_attribute_access() {
        let d = diags(r#"output "x" { value = aws_vpc.main.id }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }
}
