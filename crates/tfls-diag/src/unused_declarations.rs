//! `terraform_unused_declarations` — flag `variable`, `local`, and
//! `data` declarations in the current document that aren't
//! referenced anywhere in the same module.
//!
//! Only applies to "root" modules (those not consumed as a child by
//! any `module { source = … }` block in the workspace). A module
//! designed for reuse exposes its variables *to consumers*; flagging
//! them as unused in the module itself would be a false positive.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::module_graph::ModuleGraphLookup;

pub fn unused_declarations_diagnostics(
    body: &Body,
    rope: &Rope,
    lookup: &dyn ModuleGraphLookup,
) -> Vec<Diagnostic> {
    if !lookup.is_root_module() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        match block.ident.as_str() {
            "variable" => check_variable(block, rope, lookup, &mut out),
            "locals" => check_locals(block, rope, lookup, &mut out),
            "data" => check_data(block, rope, lookup, &mut out),
            _ => {}
        }
    }
    out
}

fn check_variable(
    block: &Block,
    rope: &Rope,
    lookup: &dyn ModuleGraphLookup,
    out: &mut Vec<Diagnostic>,
) {
    let Some(name) = first_label_str(block) else {
        return;
    };
    if lookup.variable_is_referenced(&name) {
        return;
    }
    push(
        out,
        rope,
        block.ident.span(),
        format!("variable `{name}` is declared but not used"),
    );
}

fn check_locals(
    block: &Block,
    rope: &Rope,
    lookup: &dyn ModuleGraphLookup,
    out: &mut Vec<Diagnostic>,
) {
    for inner in block.body.iter() {
        let Some(attr) = inner.as_attribute() else {
            continue;
        };
        let name = attr.key.as_str();
        if lookup.local_is_referenced(name) {
            continue;
        }
        push(
            out,
            rope,
            attr.key.span(),
            format!("local `{name}` is declared but not used"),
        );
    }
}

fn check_data(
    block: &Block,
    rope: &Rope,
    lookup: &dyn ModuleGraphLookup,
    out: &mut Vec<Diagnostic>,
) {
    let type_name = match block.labels.first() {
        Some(BlockLabel::String(s)) => s.value().as_str().to_string(),
        Some(BlockLabel::Ident(i)) => i.as_str().to_string(),
        None => return,
    };
    let name = match block.labels.get(1) {
        Some(BlockLabel::String(s)) => s.value().as_str().to_string(),
        Some(BlockLabel::Ident(i)) => i.as_str().to_string(),
        None => return,
    };
    if lookup.data_source_is_referenced(&type_name, &name) {
        return;
    }
    // Data sources used only for side effects (e.g. waiting on an
    // external resource) happen in the wild; keep the diagnostic a
    // warning, matching tflint's default.
    push(
        out,
        rope,
        block.ident.span(),
        format!("data `{type_name}.{name}` is declared but not used"),
    );
}

fn first_label_str(block: &Block) -> Option<String> {
    block.labels.first().map(|l| match l {
        BlockLabel::String(s) => s.value().as_str().to_string(),
        BlockLabel::Ident(i) => i.as_str().to_string(),
    })
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

// Avoid unused-import warnings when the lookup trait methods aren't
// explicitly called by name.
#[allow(dead_code)]
fn _force_use(_e: Expression) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tfls_parser::parse_source;

    struct FakeLookup {
        used_vars: HashSet<&'static str>,
        used_locals: HashSet<&'static str>,
        used_data: HashSet<(&'static str, &'static str)>,
        is_root: bool,
    }

    impl ModuleGraphLookup for FakeLookup {
        fn variable_is_referenced(&self, name: &str) -> bool {
            self.used_vars.contains(name)
        }
        fn local_is_referenced(&self, name: &str) -> bool {
            self.used_locals.contains(name)
        }
        fn data_source_is_referenced(&self, type_name: &str, name: &str) -> bool {
            self.used_data.contains(&(type_name, name))
        }
        fn used_provider_locals(&self) -> HashSet<String> {
            HashSet::new()
        }
        fn present_files(&self) -> HashSet<String> {
            HashSet::new()
        }
        fn is_root_module(&self) -> bool {
            self.is_root
        }
    }

    fn diags(src: &str, lookup: FakeLookup) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        unused_declarations_diagnostics(&body, &rope, &lookup)
    }

    fn unused(is_root: bool) -> FakeLookup {
        FakeLookup {
            used_vars: HashSet::new(),
            used_locals: HashSet::new(),
            used_data: HashSet::new(),
            is_root,
        }
    }

    #[test]
    fn flags_unused_variable_in_root_module() {
        let d = diags(
            r#"variable "x" { type = string }"#,
            unused(true),
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("`x`"), "got: {}", d[0].message);
    }

    #[test]
    fn silent_for_referenced_variable() {
        let d = diags(
            r#"variable "x" { type = string }"#,
            FakeLookup {
                used_vars: ["x"].into_iter().collect(),
                used_locals: HashSet::new(),
                used_data: HashSet::new(),
                is_root: true,
            },
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_in_non_root_module() {
        let d = diags(
            r#"variable "x" { type = string }"#,
            unused(false),
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_unused_local() {
        let d = diags("locals {\n  x = 1\n  y = 2\n}", unused(true));
        assert_eq!(d.len(), 2, "got: {d:?}");
    }

    #[test]
    fn silent_for_referenced_local() {
        let d = diags(
            r#"locals { x = 1 }"#,
            FakeLookup {
                used_vars: HashSet::new(),
                used_locals: ["x"].into_iter().collect(),
                used_data: HashSet::new(),
                is_root: true,
            },
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_unused_data_source() {
        let d = diags(
            r#"data "aws_ami" "ubuntu" {}"#,
            unused(true),
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("aws_ami.ubuntu"), "got: {}", d[0].message);
    }

    #[test]
    fn silent_for_referenced_data_source() {
        let d = diags(
            r#"data "aws_ami" "ubuntu" {}"#,
            FakeLookup {
                used_vars: HashSet::new(),
                used_locals: HashSet::new(),
                used_data: [("aws_ami", "ubuntu")].into_iter().collect(),
                is_root: true,
            },
        );
        assert!(d.is_empty(), "got: {d:?}");
    }
}
