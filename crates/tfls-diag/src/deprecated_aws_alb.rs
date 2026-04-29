//! `terraform_deprecated_aws_alb` — flag uses of `resource
//! "aws_alb"` (and the related `aws_alb_target_group`,
//! `aws_alb_listener`, etc., live in their own rule modules).
//!
//! AWS provider documentation has called `aws_alb` an alias of
//! `aws_lb` since the rename in early 2018; new code is
//! universally written against `aws_lb`. The two are
//! structurally identical — `aws_alb` exists for backward
//! compatibility only.
//!
//! **Provider-version gated**: the framework's first
//! `Gate::ProviderVersion` rule. Suppressed when the module's
//! `terraform { required_providers { aws = ... } }` constraint
//! excludes AWS provider 1.7.0 (the version that introduced
//! the renamed `aws_lb`). Real-world projects pinned to AWS
//! provider 1.x might genuinely need `aws_alb`; modern
//! workspaces (4.x / 5.x / 6.x) all ship `aws_lb`.
//!
//! Diagnostic-only — no auto-fix action: rename + reference
//! rewrites are mechanical but `aws_alb`'s schema and
//! `aws_lb`'s schema have very subtle differences in attribute
//! ordering and `enable_*` flag defaults; an automated rename
//! could mask edge-case schema-validation regressions. Users
//! migrate by hand.

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule, Gate};

const RULE: DeprecationRule = DeprecationRule {
    block_kind: "resource",
    label: "aws_alb",
    gate: Gate::ProviderVersion {
        provider: "aws",
        threshold: "1.7.0",
    },
    message: "`aws_alb` is a backward-compatibility alias for `aws_lb` (AWS provider 1.7+). \
              Use `aws_lb` for new code — schemas are identical, refs need updating in \
              tandem (`aws_alb.X.arn` → `aws_lb.X.arn`).",
};

pub fn deprecated_aws_alb_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics(&RULE, body, rope)
}

pub fn deprecated_aws_alb_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    supports: bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_for_module(&RULE, body, rope, supports)
}

/// AWS provider 1.7.0 introduced `aws_lb` as the canonical
/// name. Suppress when the constraint admits any pre-1.7
/// provider version.
pub fn supports_aws_lb(constraint: &str) -> bool {
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
        deprecated_aws_alb_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_aws_alb_when_unconstrained() {
        let d = diags("resource \"aws_alb\" \"x\" { name = \"x\" }\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("aws_lb"));
    }

    #[test]
    fn ignores_aws_lb() {
        let d = diags("resource \"aws_lb\" \"x\" { name = \"x\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn ignores_unrelated_resources() {
        let d = diags("resource \"aws_instance\" \"x\" { ami = \"a\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn fires_when_required_providers_admits_modern_aws() {
        let src = concat!(
            "terraform {\n  required_providers {\n    aws = \"~> 5.0\"\n  }\n}\n",
            "resource \"aws_alb\" \"x\" { name = \"x\" }\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn suppressed_when_required_providers_pins_pre_1_7() {
        let src = concat!(
            "terraform {\n  required_providers {\n    aws = \"< 1.5\"\n  }\n}\n",
            "resource \"aws_alb\" \"x\" { name = \"x\" }\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_when_required_providers_long_form_admits_modern_aws() {
        let src = concat!(
            "terraform {\n  required_providers {\n",
            "    aws = { source = \"hashicorp/aws\", version = \"~> 4.0\" }\n",
            "  }\n}\n",
            "resource \"aws_alb\" \"x\" { name = \"x\" }\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn module_aware_helper_fires_when_supports_overridden() {
        let src = concat!(
            "terraform {\n  required_providers {\n    aws = \"< 1.5\"\n  }\n}\n",
            "resource \"aws_alb\" \"x\" { name = \"x\" }\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = deprecated_aws_alb_diagnostics_for_module(&body, &rope, true);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn supports_aws_lb_predicate() {
        assert!(supports_aws_lb(">= 4.0"));
        assert!(supports_aws_lb("~> 5.0"));
        assert!(supports_aws_lb("= 1.7.0"));
        assert!(!supports_aws_lb("< 1.5"));
        assert!(!supports_aws_lb("= 1.5.0"));
    }
}
