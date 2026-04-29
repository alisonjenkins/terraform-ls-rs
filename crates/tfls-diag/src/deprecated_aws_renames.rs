//! AWS provider type renames — the family of deprecations
//! where the resource (or data source) was renamed for
//! consistency, with no schema change. Migration is purely
//! mechanical: rewrite the block label + every reference to
//! the type, optionally emit `moved { }` blocks for state
//! migration.
//!
//! Hosted as a TABLE here (rather than one module per rule)
//! because every rule shares the same shape — same gate
//! (provider AWS), same threshold for most of them, same
//! diagnostic mechanics, same eventual auto-fix mechanics.
//! Adding a new rename = one entry in `AWS_TYPE_RENAMES`,
//! one entry in `HARDCODED_DEPRECATION_LABELS`, no new file.
//!
//! Coverage so far (sourced from AWS provider release notes
//! cross-referenced against `tfls-deprecation-scrape` output
//! shape; threshold = the version at which the replacement
//! type was introduced, NOT the version that flagged the
//! original as deprecated):
//!
//! | From                              | To                              | Threshold (AWS provider) |
//! |-----------------------------------|---------------------------------|---------------------------|
//! | `aws_alb`                         | `aws_lb`                        | 1.7.0                     |
//! | `aws_alb_listener`                | `aws_lb_listener`               | 1.7.0                     |
//! | `aws_alb_listener_rule`           | `aws_lb_listener_rule`          | 1.7.0                     |
//! | `aws_alb_target_group`            | `aws_lb_target_group`           | 1.7.0                     |
//! | `aws_alb_target_group_attachment` | `aws_lb_target_group_attachment`| 1.7.0                     |
//! | `aws_s3_bucket_object`            | `aws_s3_object`                 | 4.0.0                     |
//!
//! All rules are diagnostic-only at present — the auto-fix
//! shape exists (block-label rewrite + ref rewrite + `moved`
//! generation, mirror of `null_resource → terraform_data`)
//! but isn't generalised yet. Tier-2 schema-driven warnings
//! cover any deprecations the AWS provider flags that aren't
//! in this table.

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule, Gate};

/// Every AWS rename rule, keyed for fast lookup by the
/// multi-rule body walker.
pub const AWS_TYPE_RENAMES: &[DeprecationRule] = &[
    DeprecationRule {
        block_kind: "resource",
        label: "aws_alb",
        gate: Gate::ProviderVersion {
            provider: "aws",
            threshold: "1.7.0",
        },
        message: "`aws_alb` is a backward-compatibility alias for `aws_lb` (AWS provider 1.7+). \
                  Use `aws_lb` for new code — schemas are identical, refs need updating in \
                  tandem (`aws_alb.X.arn` → `aws_lb.X.arn`).",
    },
    DeprecationRule {
        block_kind: "resource",
        label: "aws_alb_listener",
        gate: Gate::ProviderVersion {
            provider: "aws",
            threshold: "1.7.0",
        },
        message: "`aws_alb_listener` is a backward-compatibility alias for `aws_lb_listener` \
                  (AWS provider 1.7+). Use `aws_lb_listener` and update references \
                  (`aws_alb_listener.X.arn` → `aws_lb_listener.X.arn`).",
    },
    DeprecationRule {
        block_kind: "resource",
        label: "aws_alb_listener_rule",
        gate: Gate::ProviderVersion {
            provider: "aws",
            threshold: "1.7.0",
        },
        message: "`aws_alb_listener_rule` is a backward-compatibility alias for \
                  `aws_lb_listener_rule` (AWS provider 1.7+). Use `aws_lb_listener_rule`.",
    },
    DeprecationRule {
        block_kind: "resource",
        label: "aws_alb_target_group",
        gate: Gate::ProviderVersion {
            provider: "aws",
            threshold: "1.7.0",
        },
        message: "`aws_alb_target_group` is a backward-compatibility alias for \
                  `aws_lb_target_group` (AWS provider 1.7+). Use `aws_lb_target_group`.",
    },
    DeprecationRule {
        block_kind: "resource",
        label: "aws_alb_target_group_attachment",
        gate: Gate::ProviderVersion {
            provider: "aws",
            threshold: "1.7.0",
        },
        message: "`aws_alb_target_group_attachment` is a backward-compatibility alias for \
                  `aws_lb_target_group_attachment` (AWS provider 1.7+). Use \
                  `aws_lb_target_group_attachment`.",
    },
    DeprecationRule {
        block_kind: "resource",
        label: "aws_s3_bucket_object",
        gate: Gate::ProviderVersion {
            provider: "aws",
            threshold: "4.0.0",
        },
        message: "`aws_s3_bucket_object` is superseded by `aws_s3_object` (AWS provider 4.0+) — \
                  the new resource adds `force_destroy` semantics and lifecycle alignment with \
                  the rest of the v4 S3 split. Migration: rename the resource type and update \
                  references.",
    },
];

/// Body-only entry point. Computes the per-rule support flag
/// from the body's own constraints, then walks the body once
/// across the full rule table.
pub fn aws_renames_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, AWS_TYPE_RENAMES, &|rule| {
        body_supports_rule(rule, body)
    })
}

/// Module-aware entry point. Caller supplies a `rule_supported`
/// closure capturing module-aggregated constraint decisions —
/// the LSP layer pulls AWS provider constraints across siblings
/// once and dispatches to each rule's threshold from there.
pub fn aws_renames_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    rule_supported: &dyn Fn(&DeprecationRule) -> bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, AWS_TYPE_RENAMES, rule_supported)
}

/// Body-only support test — used by the convenience entry.
/// Multi-file modules should prefer the `_for_module` variant
/// since `required_providers` typically lives in `versions.tf`.
fn body_supports_rule(rule: &DeprecationRule, body: &Body) -> bool {
    let constraint = match &rule.gate {
        Gate::TerraformVersion { .. } => deprecation_rule::extract_required_version(body),
        Gate::ProviderVersion { provider, .. } => {
            deprecation_rule::extract_required_provider_version(body, provider)
        }
    };
    let Some(c) = constraint else { return true };
    deprecation_rule::supports(rule, &c)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        aws_renames_diagnostics(&body, &rope)
    }

    /// Each rule in the table must:
    /// - have a non-empty message
    /// - use `Gate::ProviderVersion { provider: "aws", ... }` (single-provider table)
    /// - use one of `resource` / `data` for block_kind (other kinds aren't supported here)
    ///
    /// Catches typos / wrong-provider entries on additions.
    #[test]
    fn rule_table_invariants() {
        for rule in AWS_TYPE_RENAMES {
            assert!(!rule.message.is_empty(), "rule {rule:?} has empty message");
            assert!(
                matches!(rule.block_kind, "resource" | "data"),
                "rule {rule:?} has unsupported block_kind"
            );
            match rule.gate {
                Gate::ProviderVersion { provider, .. } => {
                    assert_eq!(provider, "aws", "rule {rule:?} not gated on AWS");
                }
                _ => panic!("rule {rule:?} should use Gate::ProviderVersion"),
            }
        }
    }

    /// Every label in the AWS rename table should appear in
    /// `HARDCODED_DEPRECATION_LABELS` so tier-2 schema-driven
    /// warnings don't double-fire on the same block. Caught at
    /// test time so adding a rule without updating the
    /// suppression list shows up in CI, not in user-visible
    /// duplicate diagnostics.
    #[test]
    fn every_aws_rename_is_hardcoded_listed() {
        for rule in AWS_TYPE_RENAMES {
            assert!(
                deprecation_rule::is_hardcoded_deprecation(rule.block_kind, rule.label),
                "rule for `{}.{}` not in HARDCODED_DEPRECATION_LABELS — \
                 tier-2 path will duplicate this warning",
                rule.block_kind,
                rule.label,
            );
        }
    }

    #[test]
    fn flags_aws_alb_when_unconstrained() {
        let d = diags("resource \"aws_alb\" \"x\" { name = \"x\" }\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("aws_lb"));
    }

    #[test]
    fn flags_aws_alb_listener() {
        let d = diags(
            "resource \"aws_alb_listener\" \"x\" {\n  load_balancer_arn = \"a\"\n  port = 80\n}\n",
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("aws_lb_listener"));
    }

    #[test]
    fn flags_aws_alb_listener_rule() {
        let d = diags(
            "resource \"aws_alb_listener_rule\" \"x\" {\n  listener_arn = \"a\"\n}\n",
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("aws_lb_listener_rule"));
    }

    #[test]
    fn flags_aws_alb_target_group() {
        let d = diags(concat!(
            "resource \"aws_alb_target_group\" \"x\" {\n",
            "  name = \"x\"\n  port = 80\n  protocol = \"HTTP\"\n  vpc_id = \"v\"\n",
            "}\n",
        ));
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("aws_lb_target_group"));
    }

    #[test]
    fn flags_aws_alb_target_group_attachment() {
        let d = diags(concat!(
            "resource \"aws_alb_target_group_attachment\" \"x\" {\n",
            "  target_group_arn = \"a\"\n  target_id = \"t\"\n",
            "}\n",
        ));
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("aws_lb_target_group_attachment"));
    }

    #[test]
    fn flags_aws_s3_bucket_object() {
        let d = diags(
            "resource \"aws_s3_bucket_object\" \"x\" {\n  bucket = \"b\"\n  key = \"k\"\n}\n",
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("aws_s3_object"));
    }

    #[test]
    fn ignores_replacement_resources() {
        let src = concat!(
            "resource \"aws_lb\" \"a\" { name = \"a\" }\n",
            "resource \"aws_lb_listener\" \"b\" {\n  load_balancer_arn = \"a\"\n  port = 80\n}\n",
            "resource \"aws_lb_target_group\" \"c\" {\n",
            "  name = \"c\"\n  port = 80\n  protocol = \"HTTP\"\n  vpc_id = \"v\"\n",
            "}\n",
            "resource \"aws_s3_object\" \"d\" {\n  bucket = \"b\"\n  key = \"k\"\n}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn ignores_unrelated_aws_resources() {
        let d = diags("resource \"aws_instance\" \"x\" { ami = \"a\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn flags_each_block_separately() {
        let src = concat!(
            "resource \"aws_alb\" \"a\" { name = \"a\" }\n",
            "resource \"aws_alb_target_group\" \"b\" {\n",
            "  name = \"b\"\n  port = 80\n  protocol = \"HTTP\"\n  vpc_id = \"v\"\n",
            "}\n",
            "resource \"aws_s3_bucket_object\" \"c\" {\n  bucket = \"b\"\n  key = \"k\"\n}\n",
        );
        assert_eq!(diags(src).len(), 3);
    }

    #[test]
    fn s3_bucket_object_suppressed_when_aws_provider_pinned_below_4() {
        let src = concat!(
            "terraform {\n  required_providers {\n    aws = \"~> 3.0\"\n  }\n}\n",
            "resource \"aws_s3_bucket_object\" \"x\" {\n  bucket = \"b\"\n  key = \"k\"\n}\n",
        );
        let d = diags(src);
        assert!(
            d.iter().all(|d| !d.message.contains("aws_s3_object")),
            "got: {d:?}"
        );
    }

    #[test]
    fn alb_family_fires_under_modern_aws_constraint() {
        let src = concat!(
            "terraform {\n  required_providers {\n    aws = \"~> 5.0\"\n  }\n}\n",
            "resource \"aws_alb\" \"a\" { name = \"a\" }\n",
            "resource \"aws_alb_listener\" \"b\" {\n",
            "  load_balancer_arn = \"a\"\n  port = 80\n",
            "}\n",
            "resource \"aws_alb_target_group\" \"c\" {\n",
            "  name = \"c\"\n  port = 80\n  protocol = \"HTTP\"\n  vpc_id = \"v\"\n",
            "}\n",
        );
        assert_eq!(diags(src).len(), 3);
    }

    #[test]
    fn alb_family_suppressed_when_pinned_pre_1_7() {
        let src = concat!(
            "terraform {\n  required_providers {\n    aws = \"< 1.5\"\n  }\n}\n",
            "resource \"aws_alb\" \"a\" { name = \"a\" }\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn module_aware_helper_fires_when_supports_overridden() {
        let src = concat!(
            "terraform {\n  required_providers {\n    aws = \"< 1.5\"\n  }\n}\n",
            "resource \"aws_alb\" \"a\" { name = \"a\" }\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        // Override: caller declares all rules supported (sibling
        // file has the modern constraint).
        let d = aws_renames_diagnostics_for_module(&body, &rope, &|_rule| true);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn module_aware_helper_suppresses_when_no_rule_supported() {
        let src = "resource \"aws_alb\" \"x\" { name = \"x\" }\n";
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = aws_renames_diagnostics_for_module(&body, &rope, &|_rule| false);
        assert!(d.is_empty());
    }

    #[test]
    fn ignores_data_block_with_matching_label() {
        // Rules are kind-specific. `data "aws_alb"` is a real
        // data source (separate from the resource of the same
        // name) and is NOT deprecated.
        let d = diags("data \"aws_alb\" \"x\" { name = \"a\" }\n");
        assert!(d.is_empty(), "data lookup for aws_alb is not deprecated");
    }
}
