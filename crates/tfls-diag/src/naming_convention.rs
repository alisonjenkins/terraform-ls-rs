//! `terraform_naming_convention` — opt-in style rule that flags
//! block names (resources, data sources, variables, outputs, locals,
//! modules) that aren't snake_case. Defaults match tflint's:
//! `[a-z][a-z0-9_]*` (no leading digit, no UPPERCASE, no dashes).

use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn naming_convention_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        if let Some(block) = structure.as_block() {
            check_block(block, rope, &mut out);
        } else if let Some(attr) = structure.as_attribute() {
            // Top-level assignments inside a `locals { }` block are
            // handled via the `locals` block case below; this branch
            // catches raw assignments at the file's top level (none
            // are valid in Terraform, so skip).
            let _ = attr;
        }
    }
    out
}

fn check_block(block: &Block, rope: &Rope, out: &mut Vec<Diagnostic>) {
    match block.ident.as_str() {
        "locals" => {
            for inner in block.body.iter() {
                let Some(attr) = inner.as_attribute() else {
                    continue;
                };
                let name = attr.key.as_str();
                if !is_snake_case(name) {
                    push(out, rope, attr.key.span(), format!(
                        "local `{name}` should be snake_case"
                    ));
                }
            }
        }
        "resource" | "data" => {
            // labels[1] is the instance name (labels[0] is the type).
            if let Some(label) = block.labels.get(1) {
                let name = label_str(label);
                if !is_snake_case(&name) {
                    let span = label_span(label);
                    push(out, rope, span, format!(
                        "{kind} name `{name}` should be snake_case",
                        kind = block.ident.as_str()
                    ));
                }
            }
        }
        "variable" | "output" | "module" | "provider" => {
            if let Some(label) = block.labels.first() {
                let name = label_str(label);
                if !is_snake_case(&name) {
                    let span = label_span(label);
                    push(out, rope, span, format!(
                        "{kind} name `{name}` should be snake_case",
                        kind = block.ident.as_str()
                    ));
                }
            }
        }
        _ => {}
    }
}

fn label_str(label: &BlockLabel) -> String {
    match label {
        BlockLabel::String(s) => s.value().as_str().to_string(),
        BlockLabel::Ident(i) => i.as_str().to_string(),
    }
}

fn label_span(label: &BlockLabel) -> Option<std::ops::Range<usize>> {
    match label {
        BlockLabel::String(s) => s.span(),
        BlockLabel::Ident(i) => i.span(),
    }
}

fn push(
    out: &mut Vec<Diagnostic>,
    rope: &Rope,
    span: Option<std::ops::Range<usize>>,
    message: String,
) {
    let range = hcl_span_to_lsp_range(rope, span.unwrap_or(0..0)).unwrap_or_default();
    out.push(Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::INFORMATION),
        source: Some("terraform-ls-rs".to_string()),
        message,
        ..Default::default()
    });
}

/// snake_case: starts with `[a-z]`, followed by `[a-z0-9_]+`.
fn is_snake_case(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        naming_convention_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_camel_case_variable() {
        let d = diags(r#"variable "myVar" {}"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("snake_case"));
    }

    #[test]
    fn flags_kebab_case_output() {
        let d = diags(r#"output "my-output" { value = 1 }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn silent_for_snake_case_variable() {
        let d = diags(r#"variable "my_var" {}"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_resource_name() {
        let d = diags(r#"resource "aws_instance" "MyWeb" {}"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("MyWeb"));
    }

    #[test]
    fn flags_local_key_not_snake_case() {
        let d = diags("locals {\n  MyLocal = 1\n}");
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn silent_when_everything_snake_case() {
        let d = diags(
            r#"variable "my_var" {}
               resource "aws_instance" "web" {}
               output "my_out" { value = 1 }"#,
        );
        assert!(d.is_empty(), "got: {d:?}");
    }
}
