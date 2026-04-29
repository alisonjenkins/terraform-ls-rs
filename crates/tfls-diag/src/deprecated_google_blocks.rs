//! GCP (google + google-beta) provider deprecations. Most GCP
//! provider deprecations are attribute-level (caught by the
//! tier-2 schema-driven path); the few block-level ones live
//! here.
//!
//! Coverage so far:
//!
//! | From                  | Replacement                            |
//! |-----------------------|----------------------------------------|
//! | `google_dataflow_job` | `google_dataflow_flex_template_job`    |
//!
//! The Dataflow split: classic `google_dataflow_job` runs jobs
//! from a JAR or template URL with limited parameter binding;
//! the flex-template variant uses container images with rich
//! per-job overrides and is the recommended path for new jobs
//! since google provider 3.45.

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule, Gate};

pub const GOOGLE_BLOCK_DEPRECATIONS: &[DeprecationRule] = &[
    DeprecationRule {
        block_kind: "resource",
        label: "google_dataflow_job",
        gate: Gate::ProviderVersion {
            provider: "google",
            threshold: "3.45.0",
        },
        message: "`google_dataflow_job` is superseded by `google_dataflow_flex_template_job` \
                  (google provider 3.45+) — flex templates run from container images with \
                  per-job parameter overrides, the recommended path for new Dataflow jobs.",
    },
];

pub fn google_blocks_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, GOOGLE_BLOCK_DEPRECATIONS, &|rule| {
        deprecation_rule::body_supports_rule(rule, body)
    })
}

pub fn google_blocks_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    rule_supported: &dyn Fn(&DeprecationRule) -> bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, GOOGLE_BLOCK_DEPRECATIONS, rule_supported)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        google_blocks_diagnostics(&body, &rope)
    }

    #[test]
    fn rule_table_invariants() {
        for rule in GOOGLE_BLOCK_DEPRECATIONS {
            assert!(!rule.message.is_empty());
            assert_eq!(rule.block_kind, "resource");
            match rule.gate {
                Gate::ProviderVersion { provider, .. } => {
                    assert_eq!(provider, "google");
                }
                _ => panic!("rule {rule:?} should use Gate::ProviderVersion"),
            }
        }
    }

    #[test]
    fn every_rule_is_hardcoded_listed() {
        for rule in GOOGLE_BLOCK_DEPRECATIONS {
            assert!(
                deprecation_rule::is_hardcoded_deprecation(rule.block_kind, rule.label),
                "`{}.{}` not in HARDCODED_DEPRECATION_LABELS",
                rule.block_kind,
                rule.label,
            );
        }
    }

    #[test]
    fn flags_google_dataflow_job_when_unconstrained() {
        let d = diags(
            "resource \"google_dataflow_job\" \"x\" {\n  name = \"x\"\n  template_gcs_path = \"gs://b/t\"\n  temp_gcs_location = \"gs://b/tmp\"\n}\n",
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("google_dataflow_flex_template_job"));
    }

    #[test]
    fn ignores_replacement_resource() {
        let d = diags(
            "resource \"google_dataflow_flex_template_job\" \"x\" {\n  name = \"x\"\n  container_spec_gcs_path = \"gs://b/t\"\n}\n",
        );
        assert!(d.is_empty());
    }

    #[test]
    fn ignores_unrelated_resources() {
        let d = diags("resource \"google_compute_instance\" \"x\" { name = \"x\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn suppressed_when_google_provider_pinned_pre_3_45() {
        let src = concat!(
            "terraform {\n  required_providers {\n    google = \"~> 3.40\"\n  }\n}\n",
            "resource \"google_dataflow_job\" \"x\" {\n  name = \"x\"\n  template_gcs_path = \"gs://b/t\"\n  temp_gcs_location = \"gs://b/tmp\"\n}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_under_modern_google_constraint() {
        let src = concat!(
            "terraform {\n  required_providers {\n",
            "    google = { source = \"hashicorp/google\", version = \"~> 5.0\" }\n",
            "  }\n}\n",
            "resource \"google_dataflow_job\" \"x\" {\n  name = \"x\"\n  template_gcs_path = \"gs://b/t\"\n  temp_gcs_location = \"gs://b/tmp\"\n}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }
}
