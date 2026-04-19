//! `terraform_required_version` — flag a top-level `terraform { … }`
//! block that doesn't set `required_version`. Pinning the required
//! Terraform version makes the module portable and prevents users
//! from running it with CLI versions that silently ignore newer
//! syntax.

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn required_version_presence_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "terraform" {
            continue;
        }
        let has_required_version = block.body.iter().any(|s| {
            s.as_attribute()
                .is_some_and(|a| a.key.as_str() == "required_version")
        });
        if has_required_version {
            continue;
        }
        let span = block.ident.span().unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: "terraform \"required_version\" attribute is required".to_string(),
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
        required_version_presence_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_terraform_block_without_required_version() {
        let d = diags("terraform {}");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("required_version"));
    }

    #[test]
    fn silent_when_required_version_set() {
        let d = diags(r#"terraform { required_version = ">= 1.6" }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_when_no_terraform_block() {
        // A file with no terraform block at all — the rule is about
        // modules that declare one, not about requiring every file to
        // have one.
        let d = diags(r#"variable "x" { type = string }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }
}
