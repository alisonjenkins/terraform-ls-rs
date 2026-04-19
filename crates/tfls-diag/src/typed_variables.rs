//! `terraform_typed_variables` — flag `variable "name" {}` blocks
//! that don't declare a `type`. Equivalent to the tflint rule of the
//! same name; the advice is to always declare types so the module's
//! interface is explicit and `terraform plan` rejects misuse early.

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn typed_variables_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "variable" {
            continue;
        }
        let has_type = block
            .body
            .iter()
            .any(|s| s.as_attribute().is_some_and(|a| a.key.as_str() == "type"));
        if has_type {
            continue;
        }
        let name = block
            .labels
            .first()
            .and_then(|l| match l {
                hcl_edit::structure::BlockLabel::String(s) => Some(s.value().as_str().to_string()),
                hcl_edit::structure::BlockLabel::Ident(i) => Some(i.as_str().to_string()),
            })
            .unwrap_or_else(|| "?".to_string());
        let span = block.ident.span().unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: format!("`{name}` variable has no type"),
            ..Default::default()
        });
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
        typed_variables_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_variable_without_type() {
        let d = diags(r#"variable "region" {}"#);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("`region`"), "got: {}", d[0].message);
        assert!(d[0].message.contains("has no type"), "got: {}", d[0].message);
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn silent_when_type_present() {
        let d = diags(r#"variable "region" { type = string }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_when_type_is_complex_expr() {
        let d = diags(r#"variable "x" { type = object({ name = string }) }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn ignores_non_variable_blocks() {
        let d = diags(r#"resource "aws_instance" "x" {}"#);
        assert!(d.is_empty(), "got: {d:?}");
    }
}
