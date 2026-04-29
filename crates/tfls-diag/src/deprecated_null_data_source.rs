//! `terraform_deprecated_null_data_source` — flag uses of
//! `data "null_data_source"` from the unmaintained
//! `hashicorp/null` provider. Sister of `null_resource` (also
//! covered) and same darwin/arm64 binary-incompat death
//! sentence. The `hashicorp/null` provider docs explicitly
//! recommend `locals { ... }` blocks as the replacement —
//! `null_data_source` was always a workaround for not having
//! computed locals, which Terraform 0.10 (locals) and 0.12
//! (richer expressions) eliminated.
//!
//! Diagnostic-only — no auto-fix action: the migration is
//! project-specific (each `inputs`/`outputs` pair maps to a
//! local with caller-determined naming, and reference rewrites
//! `data.null_data_source.X.outputs.Y` → `local.<X_Y>` would
//! collide with naming conventions).
//!
//! Thin wrapper over [`crate::deprecation_rule`].

use hcl_edit::structure::Body;
use lsp_types::Diagnostic;
use ropey::Rope;

use crate::deprecation_rule::{self, DeprecationRule, Gate};

const RULE: DeprecationRule = DeprecationRule {
    block_kind: "data",
    label: "null_data_source",
    gate: Gate::TerraformVersion { threshold: "0.10.0" },
    message: "`data \"null_data_source\"` is part of the unmaintained `hashicorp/null` provider \
              (the bundled binary is unavailable on darwin/arm64 and several modern Linux \
              variants). Replace with a `locals { ... }` block — Terraform 0.10+ supports \
              computed locals and 0.12+ adds rich expressions covering every case \
              null_data_source was a workaround for.",
};

pub fn deprecated_null_data_source_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics(&RULE, body, rope)
}

pub fn deprecated_null_data_source_diagnostics_for_module(
    body: &Body,
    rope: &Rope,
    supports: bool,
) -> Vec<Diagnostic> {
    deprecation_rule::diagnostics_for_module(&RULE, body, rope, supports)
}

/// Locals (`locals { ... }`) landed in Terraform 0.10. Suppress
/// when the constraint admits any pre-0.10 version.
pub fn supports_locals_replacement(constraint: &str) -> bool {
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
        deprecated_null_data_source_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_null_data_source_when_unconstrained() {
        let d = diags("data \"null_data_source\" \"x\" {\n  inputs = { a = 1 }\n}\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("locals"));
        assert!(d[0].message.contains("hashicorp/null"));
    }

    #[test]
    fn ignores_other_data_sources() {
        let d = diags("data \"aws_ami\" \"x\" { most_recent = true }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn ignores_null_resource() {
        // Sister deprecation; this rule only flags null_data_source.
        let d = diags("resource \"null_resource\" \"x\" {}\n");
        assert!(d.is_empty());
    }

    #[test]
    fn suppressed_when_required_version_excludes_0_10() {
        let src = concat!(
            "terraform { required_version = \"< 0.10\" }\n",
            "data \"null_data_source\" \"x\" {\n  inputs = { a = 1 }\n}\n",
        );
        assert!(diags(src).is_empty());
    }

    #[test]
    fn fires_when_required_version_admits_0_10() {
        let src = concat!(
            "terraform { required_version = \">= 0.10\" }\n",
            "data \"null_data_source\" \"x\" {\n  inputs = { a = 1 }\n}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn fires_for_modern_constraint() {
        let src = concat!(
            "terraform { required_version = \"~> 1.5\" }\n",
            "data \"null_data_source\" \"x\" {\n  inputs = { a = 1 }\n}\n",
        );
        assert_eq!(diags(src).len(), 1);
    }

    #[test]
    fn flags_each_block_separately() {
        let src = concat!(
            "data \"null_data_source\" \"a\" {\n  inputs = { x = 1 }\n}\n",
            "data \"null_data_source\" \"b\" {\n  inputs = { y = 2 }\n}\n",
        );
        assert_eq!(diags(src).len(), 2);
    }

    #[test]
    fn module_aware_helper_fires_when_supports_overridden() {
        let src = concat!(
            "terraform { required_version = \"< 0.10\" }\n",
            "data \"null_data_source\" \"x\" {\n  inputs = { a = 1 }\n}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = deprecated_null_data_source_diagnostics_for_module(&body, &rope, true);
        assert_eq!(d.len(), 1);
    }
}
