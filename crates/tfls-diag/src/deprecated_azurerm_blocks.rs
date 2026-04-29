//! Azure (azurerm) provider deprecations. Unlike the AWS and
//! Kubernetes families, the major azurerm deprecations are
//! semantic *splits* (one resource → two OS-specific ones)
//! rather than simple renames, so the auto-fix path can't be
//! the trivial label rewrite. Diagnostic-only.
//!
//! Coverage so far:
//!
//! | From                                 | Replacement                                                                |
//! |--------------------------------------|----------------------------------------------------------------------------|
//! | `azurerm_virtual_machine`            | `azurerm_linux_virtual_machine` OR `azurerm_windows_virtual_machine`       |
//! | `azurerm_virtual_machine_scale_set`  | `azurerm_linux_virtual_machine_scale_set` OR the `_windows_` equivalent    |
//!
//! Threshold: azurerm 2.40+ introduced both new types (became
//! the recommended pair in 3.0). Suppressed for projects
//! pinned to provider versions that pre-date the split.

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule, Gate};

pub const AZURERM_BLOCK_DEPRECATIONS: &[DeprecationRule] = &[
    DeprecationRule {
        block_kind: "resource",
        label: "azurerm_virtual_machine",
        gate: Gate::ProviderVersion {
            provider: "azurerm",
            threshold: "2.40.0",
        },
        message: "`azurerm_virtual_machine` is superseded by the OS-specific pair \
                  `azurerm_linux_virtual_machine` / `azurerm_windows_virtual_machine` (azurerm \
                  2.40+; canonical from 3.0). The new resources have stricter schemas around OS \
                  configuration — the migration is not a pure rename, the body needs adjusting.",
    },
    DeprecationRule {
        block_kind: "resource",
        label: "azurerm_virtual_machine_scale_set",
        gate: Gate::ProviderVersion {
            provider: "azurerm",
            threshold: "2.40.0",
        },
        message: "`azurerm_virtual_machine_scale_set` is superseded by \
                  `azurerm_linux_virtual_machine_scale_set` / \
                  `azurerm_windows_virtual_machine_scale_set` (azurerm 2.40+). Pick the OS-specific \
                  resource that matches your `os_profile`; schema is similar but not identical.",
    },
];

pub fn azurerm_blocks_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, AZURERM_BLOCK_DEPRECATIONS, &|rule| {
        deprecation_rule::body_supports_rule(rule, body)
    })
}

pub fn azurerm_blocks_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    rule_supported: &dyn Fn(&DeprecationRule) -> bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_from_table(body, rope, AZURERM_BLOCK_DEPRECATIONS, rule_supported)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        azurerm_blocks_diagnostics(&body, &rope)
    }

    #[test]
    fn rule_table_invariants() {
        for rule in AZURERM_BLOCK_DEPRECATIONS {
            assert!(!rule.message.is_empty());
            assert_eq!(rule.block_kind, "resource");
            match rule.gate {
                Gate::ProviderVersion { provider, .. } => {
                    assert_eq!(provider, "azurerm");
                }
                _ => panic!("rule {rule:?} should use Gate::ProviderVersion"),
            }
        }
    }

    #[test]
    fn every_rule_is_hardcoded_listed() {
        for rule in AZURERM_BLOCK_DEPRECATIONS {
            assert!(
                deprecation_rule::is_hardcoded_deprecation(rule.block_kind, rule.label),
                "`{}.{}` not in HARDCODED_DEPRECATION_LABELS",
                rule.block_kind,
                rule.label,
            );
        }
    }

    #[test]
    fn flags_azurerm_virtual_machine_when_unconstrained() {
        let d = diags(
            "resource \"azurerm_virtual_machine\" \"x\" {\n  name = \"x\"\n  location = \"l\"\n  resource_group_name = \"r\"\n  vm_size = \"s\"\n}\n",
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("azurerm_linux_virtual_machine"));
        assert!(d[0].message.contains("azurerm_windows_virtual_machine"));
    }

    #[test]
    fn flags_azurerm_virtual_machine_scale_set() {
        let d = diags(
            "resource \"azurerm_virtual_machine_scale_set\" \"x\" {\n  name = \"x\"\n  location = \"l\"\n  resource_group_name = \"r\"\n}\n",
        );
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn ignores_split_replacements() {
        let src = concat!(
            "resource \"azurerm_linux_virtual_machine\" \"a\" { name = \"a\" }\n",
            "resource \"azurerm_windows_virtual_machine\" \"b\" { name = \"b\" }\n",
            "resource \"azurerm_linux_virtual_machine_scale_set\" \"c\" { name = \"c\" }\n",
            "resource \"azurerm_windows_virtual_machine_scale_set\" \"d\" { name = \"d\" }\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn ignores_unrelated_resources() {
        let d = diags("resource \"aws_instance\" \"x\" { ami = \"a\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn suppressed_when_azurerm_provider_pinned_pre_2_40() {
        let src = concat!(
            "terraform {\n  required_providers {\n    azurerm = \"~> 2.30\"\n  }\n}\n",
            "resource \"azurerm_virtual_machine\" \"x\" {\n  name = \"x\"\n  location = \"l\"\n  resource_group_name = \"r\"\n  vm_size = \"s\"\n}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_under_modern_azurerm_constraint() {
        let src = concat!(
            "terraform {\n  required_providers {\n",
            "    azurerm = { source = \"hashicorp/azurerm\", version = \"~> 3.0\" }\n",
            "  }\n}\n",
            "resource \"azurerm_virtual_machine\" \"x\" {\n  name = \"x\"\n  location = \"l\"\n  resource_group_name = \"r\"\n  vm_size = \"s\"\n}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }
}
