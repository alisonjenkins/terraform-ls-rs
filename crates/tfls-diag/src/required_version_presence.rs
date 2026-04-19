//! `terraform_required_version` — flag a module that doesn't
//! declare `required_version` anywhere. Terraform merges all
//! `terraform {}` blocks in a module at plan time, so the check
//! is module-wide rather than per-file. The warning is emitted on
//! the first `terraform {}` block we see in the current document;
//! walking a module without any `terraform` block at all yields no
//! diagnostic for the current file.

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::module_graph::ModuleGraphLookup;

pub fn required_version_presence_diagnostics(
    body: &Body,
    rope: &Rope,
    lookup: &dyn ModuleGraphLookup,
) -> Vec<Diagnostic> {
    if lookup.module_has_required_version() {
        return Vec::new();
    }
    // Only the "primary" doc for the module emits this warning, so
    // a module with N `terraform{}` blocks spread across files sees
    // one warning total, not N.
    if !lookup.is_primary_terraform_doc() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "terraform" {
            continue;
        }
        let span = block.ident.span().unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: "terraform \"required_version\" attribute is required".to_string(),
            ..Default::default()
        });
        break;
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tfls_parser::parse_source;

    struct Lookup {
        module_has_rv: bool,
    }
    impl ModuleGraphLookup for Lookup {
        fn variable_is_referenced(&self, _: &str) -> bool {
            true
        }
        fn local_is_referenced(&self, _: &str) -> bool {
            true
        }
        fn data_source_is_referenced(&self, _: &str, _: &str) -> bool {
            true
        }
        fn used_provider_locals(&self) -> HashSet<String> {
            HashSet::new()
        }
        fn present_files(&self) -> HashSet<String> {
            HashSet::new()
        }
        fn is_root_module(&self) -> bool {
            true
        }
        fn module_has_required_version(&self) -> bool {
            self.module_has_rv
        }
        fn is_primary_terraform_doc(&self) -> bool {
            true
        }
        fn providers_with_version_set(&self) -> HashSet<String> {
            HashSet::new()
        }
    }

    fn diags(src: &str, has_rv: bool) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        required_version_presence_diagnostics(
            &body,
            &rope,
            &Lookup {
                module_has_rv: has_rv,
            },
        )
    }

    #[test]
    fn flags_terraform_block_when_module_lacks_required_version() {
        let d = diags("terraform {}", false);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("required_version"));
    }

    #[test]
    fn silent_when_module_declares_required_version_elsewhere() {
        let d = diags("terraform {}", true);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn only_emits_once_per_file() {
        let d = diags("terraform {}\nterraform {}\n", false);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn silent_when_no_terraform_block() {
        let d = diags(r#"variable "x" { type = string }"#, false);
        assert!(d.is_empty(), "got: {d:?}");
    }
}
