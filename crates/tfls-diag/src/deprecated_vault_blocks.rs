//! Vault provider block deprecations. Currently covers
//! `vault_generic_secret` → `vault_kv_secret_v1` /
//! `vault_kv_secret_v2` (split based on which KV backend
//! version the path mounts under). Diagnostic-only —
//! migration target depends on the user's KV backend mount,
//! which we can't infer without a Vault API call.
//!
//! Threshold: vault provider 3.0+ — when the explicit-version
//! KV resources became the recommended path.

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule, Gate};

pub const VAULT_BLOCK_DEPRECATIONS: &[DeprecationRule] = &[
    DeprecationRule {
        block_kind: "resource",
        label: "vault_generic_secret",
        gate: Gate::ProviderVersion {
            provider: "vault",
            threshold: "3.0.0",
        },
        message: "`vault_generic_secret` is superseded by `vault_kv_secret_v1` / \
                  `vault_kv_secret_v2` (vault provider 3.0+) — pick the resource \
                  matching the mount's KV backend version. The new resources expose \
                  metadata + lease semantics the generic resource doesn't.",
    },
];

pub fn vault_blocks_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, VAULT_BLOCK_DEPRECATIONS, &|rule| {
        deprecation_rule::body_supports_rule(rule, body)
    })
}

pub fn vault_blocks_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    rule_supported: &dyn Fn(&DeprecationRule) -> bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, VAULT_BLOCK_DEPRECATIONS, rule_supported)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        vault_blocks_diagnostics(&body, &rope)
    }

    #[test]
    fn rule_table_invariants() {
        for rule in VAULT_BLOCK_DEPRECATIONS {
            assert!(!rule.message.is_empty());
            assert_eq!(rule.block_kind, "resource");
            match rule.gate {
                Gate::ProviderVersion { provider, .. } => {
                    assert_eq!(provider, "vault");
                }
                _ => panic!("rule {rule:?} should use Gate::ProviderVersion"),
            }
        }
    }

    #[test]
    fn every_rule_is_hardcoded_listed() {
        for rule in VAULT_BLOCK_DEPRECATIONS {
            assert!(
                deprecation_rule::is_hardcoded_deprecation(rule.block_kind, rule.label),
                "`{}.{}` not in HARDCODED_DEPRECATION_LABELS",
                rule.block_kind,
                rule.label,
            );
        }
    }

    #[test]
    fn flags_vault_generic_secret_when_unconstrained() {
        let d = diags(
            "resource \"vault_generic_secret\" \"x\" {\n  path = \"secret/foo\"\n  data_json = \"{}\"\n}\n",
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("vault_kv_secret_v2"));
        assert!(d[0].message.contains("vault_kv_secret_v1"));
    }

    #[test]
    fn ignores_replacements() {
        let src = concat!(
            "resource \"vault_kv_secret_v1\" \"a\" {\n  path = \"secret/a\"\n}\n",
            "resource \"vault_kv_secret_v2\" \"b\" {\n  mount = \"kv\"\n  name = \"b\"\n}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn ignores_unrelated_resources() {
        let d = diags("resource \"aws_instance\" \"x\" { ami = \"a\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn suppressed_when_vault_provider_pinned_pre_3_0() {
        let src = concat!(
            "terraform {\n  required_providers {\n    vault = \"~> 2.20\"\n  }\n}\n",
            "resource \"vault_generic_secret\" \"x\" {\n  path = \"secret/foo\"\n  data_json = \"{}\"\n}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_under_modern_vault_constraint() {
        let src = concat!(
            "terraform {\n  required_providers {\n",
            "    vault = { source = \"hashicorp/vault\", version = \"~> 4.0\" }\n",
            "  }\n}\n",
            "resource \"vault_generic_secret\" \"x\" {\n  path = \"secret/foo\"\n  data_json = \"{}\"\n}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }
}
