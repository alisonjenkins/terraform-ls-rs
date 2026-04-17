//! Diagnostics for references that don't resolve to any known
//! definition in the same document.
//!
//! This is a per-document check — workspace-wide resolution (module
//! references across files, etc.) is a future enhancement.

use lsp_types::{Diagnostic, DiagnosticSeverity};
use tfls_core::SymbolTable;
use tfls_parser::{Reference, ReferenceKind};

/// Produce diagnostics for any `var.*`/`local.*`/`module.*` reference
/// whose target is not declared in `symbols`. Resource and data-source
/// references are skipped here because unqualified `<type>.<name>`
/// references are indistinguishable from valid cross-module references
/// without workspace-wide analysis.
pub fn undefined_reference_diagnostics(
    references: &[Reference],
    symbols: &SymbolTable,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for r in references {
        if let Some(diag) = check_reference(r, symbols) {
            out.push(diag);
        }
    }
    out
}

fn check_reference(r: &Reference, symbols: &SymbolTable) -> Option<Diagnostic> {
    let (missing_kind, name) = match &r.kind {
        ReferenceKind::Variable { name } if !symbols.variables.contains_key(name) => {
            ("variable", name.as_str())
        }
        ReferenceKind::Local { name } if !symbols.locals.contains_key(name) => {
            ("local", name.as_str())
        }
        ReferenceKind::Module { name } if !symbols.modules.contains_key(name) => {
            ("module", name.as_str())
        }
        _ => return None,
    };

    Some(Diagnostic {
        range: r.location.range(),
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("terraform-ls-rs".to_string()),
        message: format!("undefined {missing_kind} `{name}`"),
        ..Default::default()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::Url;
    use ropey::Rope;
    use tfls_parser::{extract_references, extract_symbols, parse_source};

    fn uri() -> Url {
        Url::parse("file:///t.tf").expect("url")
    }

    fn analyse(src: &str) -> (SymbolTable, Vec<Reference>) {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        (
            extract_symbols(&body, &uri(), &rope),
            extract_references(&body, &uri(), &rope),
        )
    }

    #[test]
    fn no_diagnostics_for_defined_variable() {
        let (syms, refs) = analyse(r#"variable "region" {}
output "x" { value = var.region }
"#);
        assert!(undefined_reference_diagnostics(&refs, &syms).is_empty());
    }

    #[test]
    fn flags_undefined_variable() {
        let (syms, refs) = analyse(r#"output "x" { value = var.missing }"#);
        let diags = undefined_reference_diagnostics(&refs, &syms);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("missing"));
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn flags_undefined_local() {
        let (syms, refs) = analyse(r#"output "x" { value = local.unknown }"#);
        let diags = undefined_reference_diagnostics(&refs, &syms);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("local"));
    }

    #[test]
    fn flags_undefined_module() {
        let (syms, refs) = analyse(r#"output "x" { value = module.m.out }"#);
        let diags = undefined_reference_diagnostics(&refs, &syms);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("module"));
    }

    #[test]
    fn ignores_resource_references() {
        // Resource refs often cross file/module boundaries; we don't
        // flag them here.
        let (syms, refs) = analyse(r#"output "x" { value = aws_instance.web.id }"#);
        assert!(undefined_reference_diagnostics(&refs, &syms).is_empty());
    }
}
