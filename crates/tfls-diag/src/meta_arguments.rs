//! `terraform_meta_arguments` — validate the `count` / `for_each`
//! repetition meta-arguments on `resource` / `data` / `module` blocks.
//!
//! Two checks, both hard authoring mistakes Terraform rejects (or warns
//! about) but which currently reach `terraform plan` with no in-editor
//! feedback:
//!
//! - **`count` and `for_each` on the same block** — Terraform errors
//!   ("Invalid combination of `count` and `for_each`"). ERROR.
//! - **`for_each` over a tuple/list literal** — `for_each` requires a map
//!   or a set of strings; a list is a type error. WARNING with a
//!   `toset(...)` hint.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, Body};
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn meta_argument_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if !matches!(block.ident.as_str(), "resource" | "data" | "module") {
            continue;
        }
        check_block(block, rope, &mut out);
    }
    out
}

fn check_block(block: &Block, rope: &Rope, out: &mut Vec<Diagnostic>) {
    let mut count_range: Option<Range> = None;
    let mut for_each: Option<(Range, &Expression)> = None;

    for entry in block.body.iter() {
        let Some(attr) = entry.as_attribute() else {
            continue;
        };
        match attr.key.as_str() {
            "count" => count_range = attr_range(attr, rope),
            "for_each" => {
                if let Some(r) = attr_range(attr, rope) {
                    for_each = Some((r, &attr.value));
                }
            }
            _ => {}
        }
    }

    // Both set — Terraform rejects this outright. Flag the `for_each`
    // (the more recently-added one in the common refactor).
    if let (Some(_count), Some((fe_range, _))) = (&count_range, &for_each) {
        out.push(Diagnostic {
            range: *fe_range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("terraform-ls-rs".to_string()),
            message: "`count` and `for_each` cannot both be set on the same block".to_string(),
            ..Default::default()
        });
    }

    // `for_each` over a tuple/list literal — wrong type (needs map/set).
    if let Some((fe_range, value)) = for_each {
        if matches!(value, Expression::Array(_)) {
            out.push(Diagnostic {
                range: fe_range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message: "`for_each` requires a map or a set of strings, not a list — \
                          wrap the value in `toset(...)`"
                    .to_string(),
                ..Default::default()
            });
        }
    }
}

fn attr_range(attr: &hcl_edit::structure::Attribute, rope: &Rope) -> Option<Range> {
    hcl_span_to_lsp_range(rope, attr.key.span()?).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        meta_argument_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_count_and_for_each_together() {
        let d = diags(
            "resource \"aws_instance\" \"x\" {\n  count    = 2\n  for_each = toset([\"a\"])\n}\n",
        );
        let both = d
            .iter()
            .find(|d| d.message.contains("cannot both be set"))
            .expect("both-set diagnostic");
        assert_eq!(both.severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn flags_for_each_over_list_literal() {
        let d = diags("resource \"aws_instance\" \"x\" {\n  for_each = [\"a\", \"b\"]\n}\n");
        let bad = d
            .iter()
            .find(|d| d.message.contains("requires a map or a set"))
            .expect("for_each list diagnostic");
        assert_eq!(bad.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn silent_for_for_each_over_toset() {
        let d = diags("resource \"aws_instance\" \"x\" {\n  for_each = toset([\"a\"])\n}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_count_only() {
        let d = diags("resource \"aws_instance\" \"x\" {\n  count = 2\n}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_for_each_over_map() {
        let d = diags("resource \"aws_instance\" \"x\" {\n  for_each = var.instances\n}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn applies_to_module_blocks() {
        let d = diags(
            "module \"m\" {\n  source   = \"./x\"\n  count    = 1\n  for_each = var.m\n}\n",
        );
        assert!(d.iter().any(|d| d.message.contains("cannot both be set")), "got: {d:?}");
    }
}
