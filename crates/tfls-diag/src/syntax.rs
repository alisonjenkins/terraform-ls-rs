//! Convert parser errors into LSP diagnostics.

use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use tfls_parser::ParseError;

/// Produce LSP diagnostics from a slice of parse errors. Extracts
/// the parser's reported location so the squiggle lands on the real
/// problem instead of line 0 of the file.
pub fn diagnostics_for_parse_errors(errors: &[ParseError]) -> Vec<Diagnostic> {
    errors
        .iter()
        .map(|e| Diagnostic {
            range: error_range(e),
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("terraform-ls-rs".to_string()),
            message: error_message(e),
            related_information: None,
            tags: None,
            data: None,
        })
        .collect()
}

fn error_range(err: &ParseError) -> Range {
    match err {
        ParseError::Syntax { source, .. } => {
            let loc = source.location();
            // hcl-edit reports 1-based line/column; LSP is 0-based.
            let line = loc.line().saturating_sub(1) as u32;
            let col = loc.column().saturating_sub(1) as u32;
            // Highlight the single character at the error location.
            // A precise end position isn't known — extend by one so
            // the caret is visible instead of a zero-width range.
            Range {
                start: Position { line, character: col },
                end: Position {
                    line,
                    character: col + 1,
                },
            }
        }
        _ => Range {
            start: Position::new(0, 0),
            end: Position::new(0, 0),
        },
    }
}

fn error_message(err: &ParseError) -> String {
    match err {
        ParseError::Syntax { source, .. } => {
            // The outer format wraps the location in an ASCII-art
            // block — too noisy for a diagnostic. Just surface the
            // inner message.
            let msg = source.message();
            if msg.is_empty() {
                "HCL syntax error".to_string()
            } else {
                format!("HCL syntax error: {msg}")
            }
        }
        _ => err.to_string(),
    }
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

    #[test]
    fn syntax_error_points_at_error_line_not_file_start() {
        let src = "variable \"a\" {}\nvariable \"b\" {\n  type = object({\n    bad !!\n  })\n}\n";
        let parsed = parse_source(src);
        assert!(parsed.has_errors());
        let diags = diagnostics_for_parse_errors(&parsed.errors);
        let d = &diags[0];
        assert!(
            d.range.start.line > 0,
            "syntax error should point past line 0; got line {}",
            d.range.start.line
        );
    }
}
