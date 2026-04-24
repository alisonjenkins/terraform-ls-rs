//! `terraform_typed_variables` — flag `variable "name" {}` blocks
//! that don't declare a `type`. Equivalent to the tflint rule of the
//! same name; the advice is to always declare types so the module's
//! interface is explicit and `terraform plan` rejects misuse early.
//!
//! **Suppressed when the variable is unused.** If
//! `unused_declarations_diagnostics` would also fire for the same
//! variable (root module + no references), showing both
//! simultaneously leads users to fix the type first only to later
//! discover they should just delete the block. Prioritise the
//! delete-or-use signal; the type advice is irrelevant for code
//! that's about to disappear.

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::module_graph::ModuleGraphLookup;

pub fn typed_variables_diagnostics(
    body: &Body,
    rope: &Rope,
    lookup: Option<&dyn ModuleGraphLookup>,
) -> Vec<Diagnostic> {
    // Only suppress on root modules — non-root (i.e. reusable
    // child) modules DO want the type warning on unused-looking
    // variables, because "unused" is never flagged on non-root
    // (those vars are module inputs exposed to consumers).
    let suppress_when_unused = lookup.is_some_and(|l| l.is_root_module());
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "variable" {
            continue;
        }
        let has_type = block
            .body
            .iter()
            .any(|s| s.as_attribute().is_some_and(|a| a.key.as_str() == "type"));
        if has_type {
            continue;
        }
        let name = block
            .labels
            .first()
            .map(|l| match l {
                hcl_edit::structure::BlockLabel::String(s) => s.value().as_str().to_string(),
                hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
            })
            .unwrap_or_else(|| "?".to_string());
        // If the unused-declarations rule would also fire here,
        // skip the type warning — fixing the type is wasted work
        // on a variable the user is about to delete.
        if suppress_when_unused {
            if let Some(l) = lookup {
                if !l.variable_is_referenced(&name) {
                    continue;
                }
            }
        }
        let span = block.ident.span().unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: format!("`{name}` variable has no type"),
            ..Default::default()
        });
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        typed_variables_diagnostics(&body, &rope, None)
    }

    /// Stub lookup for testing the suppress-when-unused
    /// path. `referenced` names return true from
    /// `variable_is_referenced`; everything else returns false.
    struct StubLookup {
        is_root: bool,
        referenced: HashSet<&'static str>,
    }
    impl ModuleGraphLookup for StubLookup {
        fn variable_is_referenced(&self, name: &str) -> bool {
            self.referenced.contains(name)
        }
        fn local_is_referenced(&self, _: &str) -> bool { true }
        fn data_source_is_referenced(&self, _: &str, _: &str) -> bool { true }
        fn used_provider_locals(&self) -> HashSet<String> { HashSet::new() }
        fn present_files(&self) -> HashSet<String> { HashSet::new() }
        fn is_root_module(&self) -> bool { self.is_root }
        fn module_has_required_version(&self) -> bool { true }
        fn is_primary_terraform_doc(&self) -> bool { true }
        fn providers_with_version_set(&self) -> HashSet<String> { HashSet::new() }
    }

    fn diags_with_lookup(src: &str, lookup: &dyn ModuleGraphLookup) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        typed_variables_diagnostics(&body, &rope, Some(lookup))
    }

    #[test]
    fn flags_variable_without_type() {
        let d = diags(r#"variable "region" {}"#);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("`region`"), "got: {}", d[0].message);
        assert!(d[0].message.contains("has no type"), "got: {}", d[0].message);
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn silent_when_type_present() {
        let d = diags(r#"variable "region" { type = string }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_when_type_is_complex_expr() {
        let d = diags(r#"variable "x" { type = object({ name = string }) }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn ignores_non_variable_blocks() {
        let d = diags(r#"resource "aws_instance" "x" {}"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn suppresses_no_type_when_variable_is_unused_on_root() {
        // The user case: a root-module variable with neither a
        // `type` attribute nor any references. The unused
        // check would also fire; showing the type warning on
        // top sends the user fixing the wrong thing first.
        let lookup = StubLookup {
            is_root: true,
            referenced: HashSet::new(),
        };
        let d = diags_with_lookup(r#"variable "orphan" {}"#, &lookup);
        assert!(
            d.is_empty(),
            "unused var must not get a `has no type` warning: {d:?}"
        );
    }

    #[test]
    fn still_flags_no_type_when_variable_is_used_on_root() {
        // Used variables get the type warning as before — the
        // user isn't about to delete them, so asking for a
        // type is a productive hint.
        let lookup = StubLookup {
            is_root: true,
            referenced: ["region"].into_iter().collect(),
        };
        let d = diags_with_lookup(r#"variable "region" {}"#, &lookup);
        assert_eq!(d.len(), 1, "used var must still get the warning: {d:?}");
    }

    #[test]
    fn still_flags_no_type_on_non_root_module_regardless_of_usage() {
        // Non-root (reusable child) modules: `unused_declarations`
        // never fires — the variables ARE the module's public
        // interface. So the type warning shouldn't be suppressed
        // either; the variable is an input the child module
        // legitimately declares.
        let lookup = StubLookup {
            is_root: false,
            referenced: HashSet::new(),
        };
        let d = diags_with_lookup(r#"variable "cidr" {}"#, &lookup);
        assert_eq!(d.len(), 1, "non-root var must still get the warning: {d:?}");
    }

    #[test]
    fn none_lookup_preserves_old_behaviour() {
        // Callers that can't provide a lookup (one-off single-
        // doc checks, unit tests) get the old unconditional
        // behaviour — the suppression needs the cross-file
        // reference context to be meaningful.
        let d = diags(r#"variable "x" {}"#);
        assert_eq!(d.len(), 1);
    }
}
