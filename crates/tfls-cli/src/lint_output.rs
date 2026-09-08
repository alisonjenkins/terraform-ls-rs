//! Pure rendering functions for `tfls-lint`'s diagnostic output.
//!
//! Every `render_*` fn takes the same sorted `(relative path, Diagnostic)`
//! entries plus a [`Summary`] and returns a `String` — no I/O, so each is
//! unit-testable without spawning the binary. `main` picks the renderer and
//! writes the result to stdout.
//!
//! Positions are 1-based (line and column).

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

/// Aggregate counts handed to every renderer alongside the diagnostic list.
#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
    pub hints: usize,
    pub files: usize,
}

pub fn severity_word(d: &Diagnostic) -> &'static str {
    match d.severity {
        Some(DiagnosticSeverity::HINT) => "hint",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        Some(DiagnosticSeverity::WARNING) => "warning",
        _ => "error",
    }
}

pub fn code_of(d: &Diagnostic) -> String {
    match &d.code {
        Some(NumberOrString::String(s)) => s.clone(),
        Some(NumberOrString::Number(n)) => n.to_string(),
        None => String::new(),
    }
}

pub fn render_text(entries: &[(String, Diagnostic)], _summary: &Summary) -> String {
    let mut out = String::new();
    for (path, d) in entries {
        out.push_str(&format!(
            "{path}:{}:{}: {} [{}] {}\n",
            d.range.start.line + 1,
            d.range.start.character + 1,
            severity_word(d),
            code_of(d),
            d.message,
        ));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn diag(severity: DiagnosticSeverity, code: &str, message: &str, line: u32) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 2), Position::new(line, 5)),
            severity: Some(severity),
            code: Some(NumberOrString::String(code.to_string())),
            code_description: None,
            source: Some("terraform-ls-rs".to_string()),
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[test]
    fn render_text_matches_existing_format() {
        let entries = vec![
            (
                "main.tf".to_string(),
                diag(
                    DiagnosticSeverity::ERROR,
                    "terraform_syntax",
                    "bad syntax",
                    0,
                ),
            ),
            (
                "vars.tf".to_string(),
                diag(
                    DiagnosticSeverity::WARNING,
                    "terraform_unused_declarations",
                    "unused variable",
                    3,
                ),
            ),
        ];
        let summary = Summary {
            errors: 1,
            warnings: 1,
            info: 0,
            hints: 0,
            files: 2,
        };
        let text = render_text(&entries, &summary);
        assert_eq!(
            text,
            "main.tf:1:3: error [terraform_syntax] bad syntax\n\
             vars.tf:4:3: warning [terraform_unused_declarations] unused variable\n"
        );
    }
}
