//! Diagnostic: flag `variable "…" { default = X, type = T }` when the
//! inferred shape of `X` doesn't satisfy the declared type `T`.
//!
//! The canonical false-positive case we want to avoid: author has a
//! reference (`var.x`, `local.y`) or computed expression in `default`
//! whose shape we can't statically infer. Those collapse to
//! [`VariableType::Any`], and [`tfls_core::satisfies`] treats `Any`
//! as a free pass on either side.

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_core::{explain_mismatch, parse_type_expr, parse_value_shape, satisfies};
use tfls_parser::hcl_span_to_lsp_range;

pub fn variable_default_type_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "variable" {
            continue;
        }

        let mut type_expr = None;
        let mut default_expr = None;
        let mut default_span = None;
        for inner in block.body.iter() {
            let Some(attr) = inner.as_attribute() else {
                continue;
            };
            match attr.key.as_str() {
                "type" => type_expr = Some(&attr.value),
                "default" => {
                    default_expr = Some(&attr.value);
                    default_span = attr.span();
                }
                _ => {}
            }
        }

        let (Some(type_expr), Some(default_expr)) = (type_expr, default_expr) else {
            continue;
        };

        let declared = parse_type_expr(type_expr);
        let actual = parse_value_shape(default_expr);

        if satisfies(&declared, &actual) {
            continue;
        }

        let span = default_span.unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        let detail = explain_mismatch(&declared, &actual);
        let message = if detail.is_empty() {
            format!(
                "default value of type `{actual}` does not match declared type `{declared}`"
            )
        } else {
            format!("default does not match declared type `{declared}`: {detail}")
        };
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("terraform-ls-rs".to_string()),
            message,
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
        variable_default_type_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_string_type_with_object_default() {
        let d = diags(r#"variable "x" {
          type    = string
          default = {}
        }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(d[0].message.contains("string"), "got: {}", d[0].message);
    }

    #[test]
    fn flags_number_type_with_string_default() {
        let d = diags(r#"variable "x" {
          type    = number
          default = "hi"
        }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn flags_list_string_with_mixed_array() {
        let d = diags(r#"variable "x" {
          type    = list(string)
          default = [1, 2]
        }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn accepts_matching_primitive() {
        let d = diags(r#"variable "x" {
          type    = string
          default = "hi"
        }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn accepts_matching_list() {
        let d = diags(r#"variable "x" {
          type    = list(string)
          default = ["a", "b"]
        }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn accepts_any_type() {
        let d = diags(r#"variable "x" {
          type    = any
          default = {}
        }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn no_diagnostic_when_default_is_unknowable() {
        // `var.y` collapses to Any in shape inference; satisfies() passes.
        let d = diags(r#"variable "x" {
          type    = string
          default = var.y
        }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn no_diagnostic_when_no_default() {
        let d = diags(r#"variable "x" {
          type = string
        }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn no_diagnostic_when_no_type() {
        // Without `type`, terraform infers from default — no mismatch possible.
        let d = diags(r#"variable "x" {
          default = "hi"
        }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn ignores_non_variable_blocks() {
        let d = diags(r#"resource "aws_instance" "x" {
          type    = "t3.micro"
        }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn accepts_object_with_optional_missing_fields() {
        let d = diags(r#"variable "x" {
          type    = object({ a = string, b = optional(number) })
          default = { a = "y" }
        }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_object_missing_required_field() {
        let d = diags(r#"variable "x" {
          type    = object({ a = string, b = number })
          default = { a = "y" }
        }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(
            d[0].message.contains("missing field") && d[0].message.contains("`b`"),
            "expected missing-field message; got: {}",
            d[0].message
        );
    }

    #[test]
    fn flags_object_extra_field() {
        // The exact case from the user report: declared schema names
        // `name`, default supplies an unrelated `a`.
        let d = diags(r#"variable "test" {
          default = { a = "b" }
          type    = object({ name = string })
        }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(
            d[0].message.contains("unknown field") && d[0].message.contains("`a`"),
            "expected unknown-field message; got: {}",
            d[0].message
        );
        assert!(
            d[0].message.contains("missing field") && d[0].message.contains("`name`"),
            "expected missing-field message; got: {}",
            d[0].message
        );
    }

    #[test]
    fn flags_object_field_type_mismatch() {
        let d = diags(r#"variable "x" {
          type    = object({ a = string })
          default = { a = 1 }
        }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(
            d[0].message.contains("`a`") && d[0].message.contains("expected"),
            "expected field-type message; got: {}",
            d[0].message
        );
    }
}
