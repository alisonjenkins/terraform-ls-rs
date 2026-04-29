//! `terraform_deprecated_template_file` — flag uses of the
//! `data "template_file"` data source from the unmaintained
//! `hashicorp/template` provider. Its bundled binary doesn't
//! exist on darwin/arm64 + several modern linux variants —
//! every project still using it is one upgrade away from a
//! hard build failure.
//!
//! Thin wrapper over [`crate::deprecation_rule`]. Pairs with
//! the `template-file-to-templatefile` code action that
//! converts the data source to a `local` calling
//! `templatefile()` and rewrites references.

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule};

const RULE: DeprecationRule = DeprecationRule {
    block_kind: "data",
    label: "template_file",
    threshold: "0.12.0",
    message: "`data \"template_file\"` is superseded by the built-in `templatefile()` function (Terraform 0.12+) — \
              use the \"Convert template_file to templatefile()\" code action.",
};

pub fn deprecated_template_file_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics(&RULE, body, rope)
}

pub fn deprecated_template_file_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    supports: bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_for_module(&RULE, body, rope, supports)
}

/// `templatefile()` was added in Terraform 0.12. Suppress when
/// the constraint admits any pre-0.12 version.
pub fn supports_templatefile(constraint: &str) -> bool {
    deprecation_rule::supports(&RULE, constraint)
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
        let src = concat!(
            "terraform { required_version = \"< 0.11\" }\n",
            "data \"template_file\" \"x\" { template = \"hi\" }\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_for_modern_constraint() {
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
