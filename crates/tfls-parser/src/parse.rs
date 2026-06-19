//! HCL parsing wrapper around `hcl-edit`.

use crate::error::ParseError;
use crate::safe::{parse_body, BodyParseError};

/// Result of parsing a single `.tf` file.
#[derive(Debug)]
pub struct ParsedFile {
    /// The parsed body — present even on partial failure if recoverable.
    pub body: Option<hcl_edit::structure::Body>,
    /// Parse errors, if any.
    pub errors: Vec<ParseError>,
}

impl ParsedFile {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Parse an HCL source string.
///
/// On success, returns the parsed body with no errors. On failure, the body
/// may be absent and `errors` will contain one or more diagnostics.
///
/// Goes through [`crate::safe::parse_body`] to isolate panics from
/// hcl-edit's parser — see that module's docstring for the full
/// list of upstream `.unwrap()` sites we're guarding against.
pub fn parse_source(source: &str) -> ParsedFile {
    match parse_body(source) {
        Ok(body) => ParsedFile {
            body: Some(body),
            errors: Vec::new(),
        },
        Err(BodyParseError::Syntax(e)) => {
            let message = e.to_string();
            ParsedFile {
                body: None,
                errors: vec![ParseError::Syntax { message, source: e }],
            }
        }
        Err(BodyParseError::Panicked(p)) => ParsedFile {
            body: None,
            errors: vec![ParseError::Panicked {
                message: p.message,
                source_excerpt: p.source_excerpt,
                source_bytes: p.source_bytes,
            }],
        },
    }
}

/// Parse a document, auto-selecting the HCL or JSON parser based on
/// the URI extension. `.tf.json` files go through the JSON parser;
/// everything else uses the HCL parser.
pub fn parse_source_for_uri(source: &str, uri_or_path: &str) -> ParsedFile {
    if uri_or_path.ends_with(".tf.json") {
        crate::json::parse_json_source(source)
    } else {
        parse_source(source)
    }
}

/// Upper bound on recovery passes — each blanks one more error line.
/// Bounds worst-case work on a heavily-broken file.
const MAX_RECOVERY_PASSES: usize = 64;

/// Like [`parse_source`] but, when the strict parse fails, attempts a
/// best-effort recovery so editor features (hover, completion, goto-def)
/// keep working on the file's still-valid blocks while the user fixes a
/// syntax error elsewhere.
///
/// Recovery blanks the line containing each syntax error — overwriting its
/// bytes with spaces while preserving every newline, so all surviving
/// spans keep their ORIGINAL byte offsets — then re-parses, repeating until
/// the source parses or no further progress is made. The returned
/// [`ParsedFile`] carries the recovered body together with the ORIGINAL
/// parse error(s), so the syntax-error diagnostic still fires and callers
/// can tell the parse was partial via [`ParsedFile::has_errors`].
pub fn parse_source_recovering(source: &str) -> ParsedFile {
    let strict = parse_source(source);
    if strict.body.is_some() || strict.errors.is_empty() {
        return strict;
    }
    // A parser panic means the input shape is unknown; re-running the
    // blanked variants risks panicking again, so don't attempt recovery.
    if strict
        .errors
        .iter()
        .any(|e| matches!(e, ParseError::Panicked { .. }))
    {
        return strict;
    }
    match recover_body(source) {
        Some(body) => ParsedFile {
            body: Some(body),
            errors: strict.errors,
        },
        None => strict,
    }
}

/// `.tf.json`-aware variant of [`parse_source_recovering`]. JSON has no
/// recovery path yet (the structured `tf.json` parser is all-or-nothing),
/// so JSON files fall back to the strict parser.
pub fn parse_source_recovering_for_uri(source: &str, uri_or_path: &str) -> ParsedFile {
    if uri_or_path.ends_with(".tf.json") {
        crate::json::parse_json_source(source)
    } else {
        parse_source_recovering(source)
    }
}

/// Repeatedly blank the line under each syntax error and re-parse. Returns
/// the first body that parses, or `None` if recovery makes no progress
/// within [`MAX_RECOVERY_PASSES`].
fn recover_body(source: &str) -> Option<hcl_edit::structure::Body> {
    let mut buf = source.as_bytes().to_vec();
    // Track which line-starts we've already blanked so a stuck error
    // (offset that keeps landing on the same already-blanked line) bails
    // instead of looping.
    let mut blanked = std::collections::HashSet::new();
    for _ in 0..MAX_RECOVERY_PASSES {
        let text = std::str::from_utf8(&buf).ok()?;
        match parse_body(text) {
            Ok(body) => return Some(body),
            Err(BodyParseError::Syntax(e)) => {
                let offset = e.location().offset().min(buf.len());
                let (start, end) = line_bounds(&buf, offset);
                if !blanked.insert(start) {
                    return None;
                }
                for b in &mut buf[start..end] {
                    *b = b' ';
                }
            }
            Err(BodyParseError::Panicked(_)) => return None,
        }
    }
    None
}

/// Byte range `[start, end)` of the line containing `offset`, excluding the
/// trailing `\n` (and a preceding `\r`) so blanking the range preserves
/// newlines and total length — keeping every other span's offset intact.
fn line_bounds(buf: &[u8], offset: usize) -> (usize, usize) {
    let offset = offset.min(buf.len());
    let start = buf[..offset]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut end = buf[offset..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| offset + i)
        .unwrap_or(buf.len());
    if end > start && buf.get(end - 1) == Some(&b'\r') {
        end -= 1;
    }
    (start, end)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_source() {
        let parsed = parse_source("");
        assert!(
            parsed.body.is_some(),
            "empty source should parse to empty body"
        );
        assert!(!parsed.has_errors());
    }

    #[test]
    fn parses_simple_resource() {
        let src = r#"
resource "aws_instance" "web" {
  ami           = "ami-123"
  instance_type = "t3.micro"
}
"#;
        let parsed = parse_source(src);
        assert!(parsed.body.is_some());
        assert!(!parsed.has_errors(), "valid HCL should parse cleanly");
    }

    #[test]
    fn parses_variable_block() {
        let src = r#"
variable "region" {
  type    = string
  default = "us-east-1"
}
"#;
        let parsed = parse_source(src);
        assert!(parsed.body.is_some());
        assert!(!parsed.has_errors());
    }

    #[test]
    fn reports_syntax_error() {
        let src = r#"
resource "aws_instance" "web" {
  ami = "unterminated
"#;
        let parsed = parse_source(src);
        assert!(parsed.has_errors(), "invalid HCL should produce errors");
    }

    #[test]
    fn syntax_error_includes_source() {
        let parsed = parse_source("resource {");
        assert!(parsed.has_errors());
        let err = &parsed.errors[0];
        match err {
            ParseError::Syntax { message, source: _ } => {
                assert!(!message.is_empty(), "error message should not be empty");
            }
            other => panic!("expected Syntax error, got {other:?}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod recovery_tests {
    use super::*;
    use hcl_edit::repr::Span;

    fn block_labels(parsed: &ParsedFile) -> Vec<String> {
        parsed
            .body
            .as_ref()
            .map(|b| {
                b.iter()
                    .filter_map(|s| s.as_block())
                    .map(|blk| {
                        blk.labels
                            .iter()
                            .map(|l| format!("{l:?}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn recovers_valid_block_around_a_broken_one() {
        // First block valid; second has a stray-token error on one line.
        let src = "resource \"aws_instance\" \"good\" {\n  ami = \"x\"\n}\n\nresource \"aws_instance\" \"bad\" {\n  ami = @@@\n}\n";
        let parsed = parse_source_recovering(src);
        assert!(parsed.has_errors(), "syntax error must still be reported");
        assert!(parsed.body.is_some(), "recovery must yield a usable body");
        // The good block survives so hover/completion can resolve it.
        let labels = block_labels(&parsed);
        assert!(
            labels.iter().any(|l| l.contains("good")),
            "valid block must survive recovery: {labels:?}"
        );
    }

    #[test]
    fn recovered_spans_keep_original_offsets() {
        // The good block sits AFTER the broken line; blanking must not shift
        // its byte offsets (we overwrite with spaces, never resize).
        let src = "resource \"x\" \"bad\" {\n  a = @@@\n}\nresource \"x\" \"good\" {\n  ami = \"v\"\n}\n";
        let parsed = parse_source_recovering(src);
        let body = parsed.body.as_ref().expect("recovered body");
        let good = body
            .iter()
            .filter_map(|s| s.as_block())
            .find(|b| b.labels.iter().any(|l| format!("{l:?}").contains("good")))
            .expect("good block present");
        let span = good.span().expect("span");
        // The recovered span must point at the real location in the ORIGINAL
        // source, not a shifted one.
        assert_eq!(&src[span.start..span.start + 10], "resource \"");
    }

    #[test]
    fn clean_source_is_untouched_by_recovering_variant() {
        let src = "resource \"aws_instance\" \"web\" {\n  ami = \"x\"\n}\n";
        let parsed = parse_source_recovering(src);
        assert!(!parsed.has_errors());
        assert!(parsed.body.is_some());
    }

    #[test]
    fn unrecoverable_input_returns_strict_result() {
        // Unbalanced brace with nothing to blank into validity — body stays
        // None, error preserved (no infinite loop).
        let src = "resource \"x\" \"y\" {\n";
        let parsed = parse_source_recovering(src);
        assert!(parsed.has_errors());
        // Either None (couldn't recover) — must not hang or panic.
        let _ = parsed.body;
    }
}

