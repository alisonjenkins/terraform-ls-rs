//! Convert parser errors into LSP diagnostics.

use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use tfls_parser::ParseError;

/// Produce LSP diagnostics from a slice of parse errors.
///
/// For now, all parse errors are reported with a default range at the
/// start of the document — hcl-edit's errors do not currently expose
/// structured position info in a way we can convert. A future
/// iteration will extract position info from the error display.
pub fn diagnostics_for_parse_errors(errors: &[ParseError]) -> Vec<Diagnostic> {
    errors
        .iter()
        .map(|e| Diagnostic {
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 0),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("terraform-ls-rs".to_string()),
            message: e.to_string(),
            related_information: None,
            tags: None,
            data: None,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    #[test]
    fn empty_errors_yield_no_diagnostics() {
        let diags = diagnostics_for_parse_errors(&[]);
        assert!(diags.is_empty());
    }

    #[test]
    fn syntax_error_becomes_error_diagnostic() {
        let parsed = parse_source("resource {");
        assert!(parsed.has_errors());
        let diags = diagnostics_for_parse_errors(&parsed.errors);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].source.as_deref(), Some("terraform-ls-rs"));
    }
}
