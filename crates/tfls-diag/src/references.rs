//! Diagnostics for references that don't resolve to any known
//! definition visible from the referencing document.
//!
//! "Visible" is decided by the caller via a resolver closure — single-
//! document callers can close over a [`SymbolTable`]; workspace callers
//! can scope lookups to a directory (a Terraform module boundary).

use lsp_types::{Diagnostic, DiagnosticSeverity};
use tfls_core::SymbolTable;
use tfls_parser::{Reference, ReferenceKind};

/// Produce diagnostics for any `var.*` / `local.*` / `module.*` reference
/// whose target the `resolver` says is undefined. Resource and data-source
/// references are skipped here because unqualified `<type>.<name>`
/// references are indistinguishable from valid cross-module references
/// without workspace-wide analysis.
///
/// `resolver` is called with the reference's kind and should return
/// `true` if a definition is known. The closure form lets callers plug
/// in either a per-document lookup or a directory-scoped workspace one.
pub fn undefined_reference_diagnostics<F>(references: &[Reference], resolver: F) -> Vec<Diagnostic>
where
    F: Fn(&ReferenceKind) -> bool,
{
    let mut out = Vec::new();
    for r in references {
        if let Some(diag) = check_reference(r, &resolver) {
            out.push(diag);
        }
    }
    out
}

/// Convenience wrapper: treat a [`SymbolTable`] as the sole definition
/// source. Used by tests and any callsite that truly only cares about
/// one document.
pub fn undefined_reference_diagnostics_for_document(
    references: &[Reference],
    symbols: &SymbolTable,
) -> Vec<Diagnostic> {
    undefined_reference_diagnostics(references, |kind| match kind {
        ReferenceKind::Variable { name } => symbols.variables.contains_key(name),
        ReferenceKind::Local { name } => symbols.locals.contains_key(name),
        ReferenceKind::Module { name } => symbols.modules.contains_key(name),
        _ => true,
    })
}

fn check_reference<F>(r: &Reference, resolver: &F) -> Option<Diagnostic>
where
    F: Fn(&ReferenceKind) -> bool,
{
    let (missing_kind, name) = match &r.kind {
        ReferenceKind::Variable { name } if !resolver(&r.kind) => ("variable", name.as_str()),
        ReferenceKind::Local { name } if !resolver(&r.kind) => ("local", name.as_str()),
        ReferenceKind::Module { name } if !resolver(&r.kind) => ("module", name.as_str()),
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
        let (syms, refs) = analyse(
            r#"variable "region" {}
output "x" { value = var.region }
"#,
        );
        assert!(undefined_reference_diagnostics_for_document(&refs, &syms).is_empty());
    }

    #[test]
    fn flags_undefined_variable() {
        let (syms, refs) = analyse(r#"output "x" { value = var.missing }"#);
        let diags = undefined_reference_diagnostics_for_document(&refs, &syms);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("missing"));
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn flags_undefined_local() {
        let (syms, refs) = analyse(r#"output "x" { value = local.unknown }"#);
        let diags = undefined_reference_diagnostics_for_document(&refs, &syms);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("local"));
    }

    #[test]
    fn flags_undefined_module() {
        let (syms, refs) = analyse(r#"output "x" { value = module.m.out }"#);
        let diags = undefined_reference_diagnostics_for_document(&refs, &syms);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("module"));
    }

    #[test]
    fn ignores_resource_references() {
        // Resource refs often cross file/module boundaries; we don't
        // flag them here.
        let (syms, refs) = analyse(r#"output "x" { value = aws_instance.web.id }"#);
        assert!(undefined_reference_diagnostics_for_document(&refs, &syms).is_empty());
    }

    #[test]
    fn resolver_can_supply_definitions_not_in_local_table() {
        // Simulate cross-file resolution: document has no definitions, but
        // the closure reports the reference as defined elsewhere.
        let (_syms, refs) = analyse(r#"output "x" { value = module.k.out }"#);
        let diags = undefined_reference_diagnostics(&refs, |kind| {
            matches!(kind, ReferenceKind::Module { name } if name == "k")
        });
        assert!(
            diags.is_empty(),
            "resolver should satisfy cross-file module ref: {diags:?}"
        );
    }
}
