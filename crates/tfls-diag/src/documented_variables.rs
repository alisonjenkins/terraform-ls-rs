//! `terraform_documented_variables` — opt-in style rule that flags
//! `variable` blocks without a `description`. Helps ensure module
//! interfaces are self-documenting. Off by default; the completion
//! scaffold already pre-seeds `description = ""` so day-to-day this
//! just catches cases where the line was deleted.

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn documented_variables_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "variable" {
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
            message: format!("variable `{name}` has no description"),
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
        documented_variables_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_variable_without_description() {
        let d = diags(r#"variable "x" { type = string }"#);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::INFORMATION));
    }

    #[test]
    fn silent_when_description_present() {
        let d = diags("variable \"x\" {\n  type = string\n  description = \"foo\"\n}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }
}
