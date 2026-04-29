//! `terraform_deprecated_null_resource` — flag uses of
//! `resource "null_resource" "X"` in projects where the
//! Terraform 1.4+ replacement `terraform_data` is available.
//!
//! Thin wrapper over the generic [`crate::deprecation_rule`]
//! scaffolding — the rule struct carries the only shape that
//! varies per deprecation. Pairs with the
//! `null-resource-to-terraform-data` code action.

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule};

const RULE: DeprecationRule = DeprecationRule {
    block_kind: "resource",
    label: "null_resource",
    threshold: "1.4.0",
    message: "`null_resource` is superseded by the built-in `terraform_data` (Terraform 1.4+) — \
              use the \"Convert null_resource to terraform_data\" code action.",
};

pub fn deprecated_null_resource_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics(&RULE, body, rope)
}

pub fn deprecated_null_resource_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    supports: bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_for_module(&RULE, body, rope, supports)
}

/// `terraform_data` was added in Terraform 1.4. Suppress when
/// the constraint admits any pre-1.4 version.
pub fn supports_terraform_data(constraint: &str) -> bool {
    deprecation_rule::supports(&RULE, constraint)
}

/// Re-exported here so existing call sites (tfls-lsp util.rs,
/// tfls-lsp code_action.rs) keep their familiar import path.
/// Lives in [`deprecation_rule`] now.
pub fn extract_required_version(body: &Body) -> Option<String> {
    deprecation_rule::extract_required_version(body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        deprecated_null_resource_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_null_resource_when_unconstrained() {
        let d = diags("resource \"null_resource\" \"x\" {}\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("terraform_data"));
    }

    #[test]
    fn ignores_other_resources() {
        let d = diags("resource \"aws_instance\" \"x\" { ami = \"a\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn suppressed_when_required_version_excludes_1_4() {
        let src = concat!(
            "terraform { required_version = \"< 1.3\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_when_required_version_admits_1_4() {
        let src = concat!(
            "terraform { required_version = \">= 1.4\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn fires_when_required_version_pessimistic_admits_1_4() {
        let src = concat!(
            "terraform { required_version = \"~> 1.4\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn fires_when_required_version_pessimistic_min_above_1_4() {
        let src = concat!(
            "terraform { required_version = \"~> 1.5\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn suppressed_when_required_version_admits_pre_1_4() {
        let src = concat!(
            "terraform { required_version = \">= 1.0\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn flags_each_block_separately() {
        let src = concat!(
            "resource \"null_resource\" \"a\" {}\n",
            "resource \"null_resource\" \"b\" {}\n",
        );
        assert_eq!(diags(src).len(), 2);
    }

    #[test]
    fn suppressed_when_exact_pin_below_1_4() {
        let src = concat!(
            "terraform { required_version = \"= 1.3.5\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_when_exact_pin_at_1_4() {
        let src = concat!(
            "terraform { required_version = \"= 1.4.0\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn suppressed_when_pre_0_11_pin() {
        let src = concat!(
            "terraform { required_version = \"< 0.11\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn suppressed_when_upper_bound_below_1_4() {
        let src = concat!(
            "terraform { required_version = \"<= 1.3.99\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn module_aware_helper_fires_when_supports_overridden() {
        let src = concat!(
            "terraform { required_version = \"< 1.3\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = deprecated_null_resource_diagnostics_for_module(&body, &rope, true);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn module_aware_helper_suppresses_when_supports_false() {
        let src = "resource \"null_resource\" \"x\" {}\n";
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = deprecated_null_resource_diagnostics_for_module(&body, &rope, false);
        assert!(d.is_empty());
    }
}
