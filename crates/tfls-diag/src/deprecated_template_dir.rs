//! `terraform_deprecated_template_dir` — flag uses of the
//! `data "template_dir"` data source from the unmaintained
//! `hashicorp/template` provider. Same darwin/arm64 binary-
//! incompat death sentence as `template_file`. Unlike its
//! sibling, there's no 1-line built-in replacement — the
//! migration pattern is `for_each = fileset(...) +
//! templatefile()` over a `local_file` resource. Diagnostic-
//! only.
//!
//! Thin wrapper over [`crate::deprecation_rule`].

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule, Gate};

const RULE: DeprecationRule = DeprecationRule {
    block_kind: "data",
    label: "template_dir",
    gate: Gate::TerraformVersion { threshold: "0.12.0" },
    message: "`data \"template_dir\"` is part of the unmaintained `hashicorp/template` provider \
              (the bundled binary is unavailable on darwin/arm64 and several modern Linux \
              variants). Migrate to `for_each = fileset(<src_dir>, \"**\")` over a `local_file` \
              resource calling `templatefile()` per match — Terraform 0.12+ ships both functions.",
};

pub fn deprecated_template_dir_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics(&RULE, body, rope)
}

pub fn deprecated_template_dir_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    supports: bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_for_module(&RULE, body, rope, supports)
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
