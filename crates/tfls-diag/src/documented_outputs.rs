//! `terraform_documented_outputs` — opt-in style rule that flags
//! `output` blocks without a `description`. Counterpart to
//! `documented_variables`; same rationale (self-documenting module
//! interface) and same default (off unless `style_rules` is enabled).

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn documented_outputs_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "output" {
            continue;
        }
        let has_description = block.body.iter().any(|s| {
            s.as_attribute()
                .is_some_and(|a| a.key.as_str() == "description")
        });
        if has_description {
            continue;
        }
        let name = block
            .labels
            .first()
            .map(|l| match l {
                hcl_edit::structure::BlockLabel::String(s) => {
                    s.value().as_str().to_string()
                }
                hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
            })
            .unwrap_or_else(|| "?".to_string());
        let span = block.ident.span().unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::INFORMATION),
            source: Some("terraform-ls-rs".to_string()),
            message: format!("output `{name}` has no description"),
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
        documented_outputs_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_output_without_description() {
        let d = diags(r#"output "x" { value = 1 }"#);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn silent_when_description_present() {
        let d = diags("output \"x\" {\n  value = 1\n  description = \"foo\"\n}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }
}
