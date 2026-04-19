//! `terraform_standard_module_structure` — flag a module directory
//! that doesn't use the conventional file layout:
//!
//! - `main.tf` — for resources, data blocks, and module calls
//! - `variables.tf` — for variable blocks
//! - `outputs.tf` — for output blocks
//!
//! The rule only fires when the module *contains* the corresponding
//! kind of declaration and hasn't put it in the conventional file:
//! declaring variables anywhere but `variables.tf` (and not in
//! `main.tf`) is the typical tflint signal. Implementation here
//! mirrors that semantic closely: warn once per offending
//! declaration type, on the current document.

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::module_graph::ModuleGraphLookup;

/// The `current_file` argument is just the filename (not the path),
/// e.g. `"main.tf"` — used to decide whether a given declaration is
/// misfiled.
pub fn standard_module_structure_diagnostics(
    body: &Body,
    rope: &Rope,
    current_file: &str,
    lookup: &dyn ModuleGraphLookup,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let present = lookup.present_files();
    // `variables.tf` / `outputs.tf` are the expected homes for their
    // kinds. Declarations living in other files are allowed to live
    // in `main.tf`; emit a diagnostic only when they've ended up
    // somewhere else. If the standard file is missing entirely from
    // the module, flag the declaration so the user knows to create
    // it.
    let mut flagged_variable = false;
    let mut flagged_output = false;
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        match block.ident.as_str() {
            "variable" => {
                if flagged_variable {
                    continue;
                }
                if should_flag(current_file, "variables.tf", &present) {
                    flagged_variable = true;
                    push(
                        &mut out,
                        rope,
                        block.ident.span(),
                        "variable declarations should live in `variables.tf`".to_string(),
                    );
                }
            }
            "output" => {
                if flagged_output {
                    continue;
                }
                if should_flag(current_file, "outputs.tf", &present) {
                    flagged_output = true;
                    push(
                        &mut out,
                        rope,
                        block.ident.span(),
                        "output declarations should live in `outputs.tf`".to_string(),
                    );
                }
            }
            _ => {}
        }
    }
    out
}

fn should_flag(
    current_file: &str,
    expected_file: &str,
    present: &std::collections::HashSet<String>,
) -> bool {
    if current_file == expected_file {
        return false;
    }
    // If the expected file exists elsewhere in the module, the
    // author knows where these belong — flag the out-of-place copy.
    // If it doesn't exist yet, flagging in the current file prompts
    // the user to create it.
    if !present.contains(expected_file) {
        return true;
    }
    // The expected file is present and we're not in it — the
    // declaration here is misplaced.
    true
}

fn push(
    out: &mut Vec<Diagnostic>,
    rope: &Rope,
    span: Option<std::ops::Range<usize>>,
    message: String,
) {
    let range = hcl_span_to_lsp_range(rope, span.unwrap_or(0..0)).unwrap_or_default();
    out.push(Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("terraform-ls-rs".to_string()),
        message,
        ..Default::default()
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tfls_parser::parse_source;

    struct FakeLookup {
        files: HashSet<String>,
    }
    impl ModuleGraphLookup for FakeLookup {
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
            self.files.clone()
        }
        fn is_root_module(&self) -> bool {
            true
        }
    }

    fn diags(src: &str, current_file: &str, files: &[&str]) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let lookup = FakeLookup {
            files: files.iter().map(|s| s.to_string()).collect(),
        };
        standard_module_structure_diagnostics(&body, &rope, current_file, &lookup)
    }

    #[test]
    fn silent_when_variables_are_in_variables_tf() {
        let d = diags(
            r#"variable "x" {}"#,
            "variables.tf",
            &["main.tf", "variables.tf"],
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_variable_in_main_tf_when_variables_tf_exists() {
        let d = diags(
            r#"variable "x" {}"#,
            "main.tf",
            &["main.tf", "variables.tf"],
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("variables.tf"));
    }

    #[test]
    fn flags_variable_when_variables_tf_missing() {
        let d = diags(r#"variable "x" {}"#, "main.tf", &["main.tf"]);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn only_one_diagnostic_per_kind_per_file() {
        let d = diags(
            "variable \"x\" {}\nvariable \"y\" {}\n",
            "main.tf",
            &["main.tf", "variables.tf"],
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn silent_when_outputs_in_outputs_tf() {
        let d = diags(
            r#"output "x" { value = 1 }"#,
            "outputs.tf",
            &["main.tf", "outputs.tf"],
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_output_in_main_tf_when_outputs_tf_exists() {
        let d = diags(
            r#"output "x" { value = 1 }"#,
            "main.tf",
            &["main.tf", "outputs.tf"],
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("outputs.tf"));
    }
}
