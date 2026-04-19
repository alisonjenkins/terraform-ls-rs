//! `terraform_comment_syntax` — opt-in style rule that flags `//`
//! comments in favor of `#`. HCL accepts both, but the Terraform
//! style guide prefers `#`.
//!
//! Implementation scans the raw source text rather than the parsed
//! tree because comments don't round-trip through `hcl-edit` as
//! structural nodes we can iterate.

use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use ropey::Rope;

pub fn comment_syntax_diagnostics(rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let source = rope.to_string();
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    let mut in_block_comment = false;
    let mut in_line_comment = false;
    let mut line: u32 = 0;
    let mut col: u32 = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'\n' {
                line += 1;
                col = 0;
                i += 1;
                continue;
            }
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                col += 2;
                continue;
            }
            i += 1;
            col += 1;
            continue;
        }
        if in_string {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                col += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            if b == b'\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                i += 1;
                col += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Flag this `//` comment.
                out.push(Diagnostic {
                    range: Range {
                        start: Position { line, character: col },
                        end: Position { line, character: col + 2 },
                    },
                    severity: Some(DiagnosticSeverity::INFORMATION),
                    source: Some("terraform-ls-rs".to_string()),
                    message: "use `#` for comments instead of `//`".to_string(),
                    ..Default::default()
                });
                in_line_comment = true;
                i += 2;
                col += 2;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // `/* ... */` is also non-idiomatic but we don't
                // flag it — tflint treats only `//` as wrong, and
                // block comments have their uses (disabling a
                // chunk of config mid-file).
                in_block_comment = true;
                i += 2;
                col += 2;
            }
            b'#' => {
                // Idiomatic comment; just skip to end of line.
                while i < bytes.len() && bytes[i] != b'\n' {
                    col += 1;
                    i += 1;
                }
            }
            b'\n' => {
                line += 1;
                col = 0;
                i += 1;
            }
            _ => {
                col += 1;
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        comment_syntax_diagnostics(&rope)
    }

    #[test]
    fn flags_line_comment_with_slashes() {
        let d = diags("// this is a comment\nvariable \"x\" {}\n");
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("#"));
    }

    #[test]
    fn silent_for_hash_comment() {
        let d = diags("# this is a comment\nvariable \"x\" {}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn does_not_flag_slashes_inside_string() {
        let d = diags(r#"variable "x" { default = "http://example.com" }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_block_comment() {
        let d = diags("/* disabled for now */\nvariable \"x\" {}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_inline_slash_comment() {
        let d = diags("variable \"x\" {} // trailing\n");
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn flags_multiple_comments() {
        let d = diags("// a\n// b\n// c\n");
        assert_eq!(d.len(), 3, "got: {d:?}");
    }
}
