//! Kubernetes provider type renames — the family where the
//! recommended canonical resource name picked up an explicit
//! Kubernetes API version suffix (`_v1`, `_v2`, etc.) so the
//! Terraform type tracks API stability rather than implicitly
//! drifting with the provider's default.
//!
//! Migration is a pure rename: `kubernetes_pod` →
//! `kubernetes_pod_v1`, schema unchanged. Pulled into one
//! consolidated table because the family is large (~20 rules)
//! and every entry shares the same gate (kubernetes provider
//! 2.0+) and shape.
//!
//! Only the well-stabilised v1 renames are listed here. Some
//! resources have additional `_v2` / `_v2beta2` variants whose
//! migration is more nuanced (HPA's autoscaling/v2 metric
//! shape isn't a drop-in for autoscaling/v1) — those are left
//! to the schema-driven tier-2 path until we have a richer
//! tier-1 design.

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule, Gate};

/// `kubernetes_*` → `kubernetes_*_v1` renames.
///
/// The migration is uniform across the family: append `_v1` to
/// the type name, schema unchanged. Message body is identical
/// per rule — the diagnostic's range covers the specific type
/// label the user wrote, so editors highlight which one
/// triggered the warning. Saves 20 hand-rolled message strings.
pub const KUBERNETES_TYPE_RENAMES: &[DeprecationRule] = &[
    rename_rule("kubernetes_pod"),
    rename_rule("kubernetes_deployment"),
    rename_rule("kubernetes_service"),
    rename_rule("kubernetes_namespace"),
    rename_rule("kubernetes_config_map"),
    rename_rule("kubernetes_secret"),
    rename_rule("kubernetes_role"),
    rename_rule("kubernetes_role_binding"),
    rename_rule("kubernetes_cluster_role"),
    rename_rule("kubernetes_cluster_role_binding"),
    rename_rule("kubernetes_persistent_volume"),
    rename_rule("kubernetes_persistent_volume_claim"),
    rename_rule("kubernetes_service_account"),
    rename_rule("kubernetes_stateful_set"),
    rename_rule("kubernetes_daemonset"),
    rename_rule("kubernetes_job"),
    rename_rule("kubernetes_cron_job"),
    rename_rule("kubernetes_network_policy"),
    rename_rule("kubernetes_ingress"),
    rename_rule("kubernetes_horizontal_pod_autoscaler"),
];

/// Builds a `kubernetes_X` rename rule. Replacement is always
/// `<from>_v1` — the user hits the diagnostic on `<from>` and
/// the message names the convention; explicit `to` parameter
/// would just duplicate the formulaic suffix in every entry.
const fn rename_rule(from: &'static str) -> DeprecationRule {
    DeprecationRule {
        block_kind: "resource",
        label: from,
        gate: Gate::ProviderVersion {
            provider: "kubernetes",
            threshold: "2.0.0",
        },
        message: "Kubernetes resource type is deprecated in favour of its API-versioned variant \
                  (kubernetes provider 2.0+). Replace with the `_v1`-suffixed type — schema is \
                  identical, references need updating in tandem.",
    }
}

/// Body-only entry point. Computes per-rule support from
/// in-body constraints. Multi-file modules should prefer the
/// LSP-layer module-aggregated path.
pub fn kubernetes_renames_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, KUBERNETES_TYPE_RENAMES, &|rule| {
        deprecation_rule::body_supports_rule(rule, body)
    })
}

/// Module-aware entry point.
pub fn kubernetes_renames_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    rule_supported: &dyn Fn(&DeprecationRule) -> bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, KUBERNETES_TYPE_RENAMES, rule_supported)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        kubernetes_renames_diagnostics(&body, &rope)
    }

    #[test]
    fn rule_table_invariants() {
        for rule in KUBERNETES_TYPE_RENAMES {
            assert!(!rule.message.is_empty(), "rule {rule:?} has empty message");
            assert_eq!(rule.block_kind, "resource");
            match rule.gate {
                Gate::ProviderVersion { provider, .. } => {
                    assert_eq!(provider, "kubernetes");
                }
                _ => panic!("rule {rule:?} should use Gate::ProviderVersion"),
            }
        }
    }

    /// Every rename in the table must appear in
    /// `HARDCODED_DEPRECATION_LABELS` so tier-2 schema-driven
    /// warnings don't double-fire.
    #[test]
    fn every_rename_is_hardcoded_listed() {
        for rule in KUBERNETES_TYPE_RENAMES {
            assert!(
                deprecation_rule::is_hardcoded_deprecation(rule.block_kind, rule.label),
                "rule for `{}.{}` not in HARDCODED_DEPRECATION_LABELS",
                rule.block_kind,
                rule.label,
            );
        }
    }

    #[test]
    fn flags_kubernetes_pod_when_unconstrained() {
        let d = diags(
            "resource \"kubernetes_pod\" \"x\" {\n  metadata {\n    name = \"x\"\n  }\n}\n",
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("_v1"));
    }

    #[test]
    fn flags_kubernetes_deployment() {
        let d = diags(
            "resource \"kubernetes_deployment\" \"x\" {\n  metadata {\n    name = \"x\"\n  }\n}\n",
        );
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn flags_kubernetes_ingress() {
        let d = diags(
            "resource \"kubernetes_ingress\" \"x\" {\n  metadata {\n    name = \"x\"\n  }\n}\n",
        );
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn ignores_v1_replacements() {
        let src = concat!(
            "resource \"kubernetes_pod_v1\" \"x\" {\n  metadata {\n    name = \"x\"\n  }\n}\n",
            "resource \"kubernetes_deployment_v1\" \"y\" {\n  metadata {\n    name = \"y\"\n  }\n}\n",
            "resource \"kubernetes_service_v1\" \"z\" {\n  metadata {\n    name = \"z\"\n  }\n}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn ignores_unrelated_resources() {
        let d = diags("resource \"aws_instance\" \"x\" { ami = \"a\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn flags_each_block_separately() {
        let src = concat!(
            "resource \"kubernetes_pod\" \"a\" {\n  metadata {\n    name = \"a\"\n  }\n}\n",
            "resource \"kubernetes_deployment\" \"b\" {\n  metadata {\n    name = \"b\"\n  }\n}\n",
            "resource \"kubernetes_service\" \"c\" {\n  metadata {\n    name = \"c\"\n  }\n}\n",
        );
        assert_eq!(diags(src).len(), 3);
    }

    #[test]
    fn suppressed_when_kubernetes_provider_pinned_pre_2_0() {
        let src = concat!(
            "terraform {\n  required_providers {\n    kubernetes = \"~> 1.13\"\n  }\n}\n",
            "resource \"kubernetes_pod\" \"x\" {\n  metadata {\n    name = \"x\"\n  }\n}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_under_modern_kubernetes_constraint() {
        let src = concat!(
            "terraform {\n  required_providers {\n",
            "    kubernetes = { source = \"hashicorp/kubernetes\", version = \"~> 2.20\" }\n",
            "  }\n}\n",
            "resource \"kubernetes_pod\" \"x\" {\n  metadata {\n    name = \"x\"\n  }\n}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn module_aware_helper_fires_when_supports_overridden() {
        let src =
            "resource \"kubernetes_pod\" \"x\" {\n  metadata {\n    name = \"x\"\n  }\n}\n";
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = kubernetes_renames_diagnostics_for_module(&body, &rope, &|_rule| true);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn module_aware_helper_suppresses_when_no_rule_supported() {
        let src =
            "resource \"kubernetes_pod\" \"x\" {\n  metadata {\n    name = \"x\"\n  }\n}\n";
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = kubernetes_renames_diagnostics_for_module(&body, &rope, &|_rule| false);
        assert!(d.is_empty());
    }

    /// Walks the entire table and asserts each rule's `from`
    /// label is recognised by the body walker. Catches typos in
    /// the table that other tests might not exercise.
    #[test]
    fn every_rule_fires_when_unconstrained() {
        for rule in KUBERNETES_TYPE_RENAMES {
            let src = format!(
                "resource \"{}\" \"x\" {{\n  metadata {{\n    name = \"x\"\n  }}\n}}\n",
                rule.label
            );
            let d = diags(&src);
            assert_eq!(
                d.len(),
                1,
                "rule {:?} did not fire for src {src:?}",
                rule.label
            );
        }
    }
}
