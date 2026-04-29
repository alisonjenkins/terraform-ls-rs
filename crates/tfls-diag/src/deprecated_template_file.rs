//! `terraform_deprecated_template_file` — flag uses of the
//! `data "template_file"` data source in projects where the
//! Terraform 0.12+ replacement `templatefile()` function is
//! available. The hashicorp/template provider is unmaintained
//! and tries to pull a binary that doesn't exist on darwin/arm64
//! and several modern linux variants — every project still
//! using it is one upgrade away from a hard build failure.
//!
//! Version-aware: suppressed when the module's `terraform { }`
//! block carries a `required_version` constraint that EXCLUDES
//! 0.12.0 (a 0.11-pinned project literally can't call
//! `templatefile()`).
//!
//! Pairs with the `template-file-to-templatefile` code action.

use hcl_edit::repr::Span;
use hcl_edit::structure::{Body, BlockLabel};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn deprecated_template_file_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecated_template_file_diagnostics_for_module(body, rope, templatefile_supported(body))
}

/// Module-aware variant. Caller supplies the precomputed
/// `supports_templatefile` decision aggregated across every
/// sibling `.tf` in the module (constraints typically live in
/// `versions.tf`, not the file the user is editing).
pub fn deprecated_template_file_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    supports: bool,
) -> Vec<Diagnostic> {
    if !supports {
        return Vec::new();
    }

    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "data" {
            continue;
        }
        let Some(label) = block.labels.first() else {
            continue;
        };
        if label_str(label) != Some("template_file") {
            continue;
        }
        let Some(span) = label.span() else { continue };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: "`data \"template_file\"` is superseded by the built-in `templatefile()` function (Terraform 0.12+) — \
                     use the \"Convert template_file to templatefile()\" code action."
                .to_string(),
            ..Default::default()
        });
    }
    out
}

/// `templatefile()` was added in Terraform 0.12. Suppress when
/// the constraint admits any pre-0.12 version. Same shape as
/// `supports_terraform_data` but with a 0.12.0 floor.
pub fn supports_templatefile(constraint: &str) -> bool {
    let parsed = tfls_core::version_constraint::parse(constraint);
    if parsed.constraints.is_empty() {
        return true;
    }
    let Some(min) = tfls_core::version_constraint::min_admitted_version(&parsed.constraints)
    else {
        return false;
    };
    tfls_core::version_constraint::version_at_least(min, "0.12.0")
}

fn templatefile_supported(body: &Body) -> bool {
    let Some(constraint) = crate::deprecated_null_resource::extract_required_version(body)
    else {
        return true;
    };
    supports_templatefile(&constraint)
}

fn label_str(label: &BlockLabel) -> Option<&str> {
    match label {
        BlockLabel::String(s) => Some(s.value().as_str()),
        BlockLabel::Ident(i) => Some(i.as_str()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        deprecated_template_file_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_template_file_when_unconstrained() {
        let d = diags("data \"template_file\" \"x\" { template = \"hi\" }\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("templatefile()"));
    }

    #[test]
    fn ignores_other_data_sources() {
        let d = diags("data \"aws_ami\" \"x\" { most_recent = true }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn suppressed_when_required_version_excludes_0_12() {
        let src = concat!(
            "terraform { required_version = \"< 0.12\" }\n",
            "data \"template_file\" \"x\" { template = \"hi\" }\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_when_required_version_admits_0_12() {
        let src = concat!(
            "terraform { required_version = \">= 0.12\" }\n",
            "data \"template_file\" \"x\" { template = \"hi\" }\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn suppressed_when_pre_0_11_pin() {
        // `< 0.11` admits 0.10.x → no templatefile().
        let src = concat!(
            "terraform { required_version = \"< 0.11\" }\n",
            "data \"template_file\" \"x\" { template = \"hi\" }\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_for_modern_constraint() {
        // `~> 1.5` → min 1.5, well past 0.12.
        let src = concat!(
            "terraform { required_version = \"~> 1.5\" }\n",
            "data \"template_file\" \"x\" { template = \"hi\" }\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn flags_each_block_separately() {
        let src = concat!(
            "data \"template_file\" \"a\" { template = \"a\" }\n",
            "data \"template_file\" \"b\" { template = \"b\" }\n",
        );
        assert_eq!(diags(src).len(), 2);
    }

    #[test]
    fn module_aware_helper_fires_when_supports_overridden() {
        let src = concat!(
            "terraform { required_version = \"< 0.11\" }\n",
            "data \"template_file\" \"x\" { template = \"a\" }\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = deprecated_template_file_diagnostics_for_module(&body, &rope, true);
        assert_eq!(d.len(), 1);
    }
}
