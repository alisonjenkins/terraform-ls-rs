//! `terraform_deprecated_template_dir` — flag uses of the
//! `data "template_dir"` data source from the unmaintained
//! `hashicorp/template` provider. Like its sibling
//! `template_file`, the provider bundles a binary that doesn't
//! exist on darwin/arm64 + several modern linux variants, so
//! every project still using it is one toolchain upgrade away
//! from a hard build failure.
//!
//! Unlike `template_file`, there's no 1-line built-in
//! replacement: the canonical migration is
//! `for_each = fileset(<dir>, "**")` + `templatefile()` per
//! match, written manually. Diagnostic-only — no auto-fix
//! action.
//!
//! Version-aware: gate matches `templatefile()`'s 0.12.0 floor
//! (the migration pattern relies on both `templatefile()` and
//! `fileset()`, both of which landed in 0.12).

use hcl_edit::repr::Span;
use hcl_edit::structure::{Body, BlockLabel};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn deprecated_template_dir_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecated_template_dir_diagnostics_for_module(
        body,
        rope,
        templatefile_supported(body),
    )
}

/// Module-aware variant. Caller supplies the precomputed
/// `supports_templatefile` decision aggregated across every
/// sibling `.tf` in the module.
pub fn deprecated_template_dir_diagnostics_for_module(
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
        if label_str(label) != Some("template_dir") {
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
            message: "`data \"template_dir\"` is part of the unmaintained `hashicorp/template` provider \
                     (the bundled binary is unavailable on darwin/arm64 and several modern Linux \
                     variants). Migrate to `for_each = fileset(<src_dir>, \"**\")` over a `local_file` \
                     resource calling `templatefile()` per match — Terraform 0.12+ ships both functions."
                .to_string(),
            ..Default::default()
        });
    }
    out
}

fn templatefile_supported(body: &Body) -> bool {
    let Some(constraint) = crate::deprecated_null_resource::extract_required_version(body)
    else {
        return true;
    };
    crate::deprecated_template_file::supports_templatefile(&constraint)
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
        deprecated_template_dir_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_template_dir_when_unconstrained() {
        let d = diags("data \"template_dir\" \"x\" {\n  source_dir = \"./tpls\"\n  destination_dir = \"./out\"\n}\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("hashicorp/template"));
        assert!(d[0].message.contains("templatefile"));
        assert!(d[0].message.contains("fileset"));
    }

    #[test]
    fn ignores_other_data_sources() {
        let d = diags("data \"aws_ami\" \"x\" { most_recent = true }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn ignores_template_file() {
        // Sibling deprecation lives in its own module; this one
        // only flags `template_dir`.
        let d = diags("data \"template_file\" \"x\" { template = \"hi\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn suppressed_when_required_version_excludes_0_12() {
        let src = concat!(
            "terraform { required_version = \"< 0.12\" }\n",
            "data \"template_dir\" \"x\" {\n  source_dir = \"./t\"\n  destination_dir = \"./o\"\n}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_when_required_version_admits_0_12() {
        let src = concat!(
            "terraform { required_version = \">= 0.12\" }\n",
            "data \"template_dir\" \"x\" {\n  source_dir = \"./t\"\n  destination_dir = \"./o\"\n}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn flags_each_block_separately() {
        let src = concat!(
            "data \"template_dir\" \"a\" {\n  source_dir = \"a\"\n  destination_dir = \"out\"\n}\n",
            "data \"template_dir\" \"b\" {\n  source_dir = \"b\"\n  destination_dir = \"out\"\n}\n",
        );
        assert_eq!(diags(src).len(), 2);
    }

    #[test]
    fn module_aware_helper_fires_when_supports_overridden() {
        let src = concat!(
            "terraform { required_version = \"< 0.11\" }\n",
            "data \"template_dir\" \"x\" {\n  source_dir = \"./t\"\n  destination_dir = \"./o\"\n}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = deprecated_template_dir_diagnostics_for_module(&body, &rope, true);
        assert_eq!(d.len(), 1);
    }
}
