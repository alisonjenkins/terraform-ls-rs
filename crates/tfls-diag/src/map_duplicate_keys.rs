//! `terraform_map_duplicate_keys` — flag object literals that
//! declare the same key twice. Terraform silently keeps the last
//! occurrence, which hides the author's intent and usually masks a
//! bug.
//!
//! Implementation note: `hcl_edit`'s `Object` type deduplicates
//! entries at parse time (the underlying `VecMap` insert overwrites
//! on collision), so we can't detect duplicates by iterating the
//! parsed representation. Instead we scan the **raw source text** of
//! each object literal — the span is recorded on the `Expression`,
//! so we slice it from the rope and tokenize ourselves just enough
//! to pull out top-level key names.

use std::collections::HashMap;

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::expr_walk::for_each_expression;

pub fn map_duplicate_keys_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let source = rope.to_string();
    for_each_expression(body, |expr| {
        let Expression::Object(_) = expr else {
            return;
        };
        let Some(span) = expr.span() else {
            return;
        };
        if span.end > source.len() {
            return;
        }
        let slice = &source[span.clone()];
        let keys = extract_top_level_keys(slice, span.start);
        let mut seen: HashMap<String, std::ops::Range<usize>> = HashMap::new();
        for (name, key_span) in keys {
            if let Some(_prior) = seen.get(&name) {
                let range = hcl_span_to_lsp_range(rope, key_span).unwrap_or_default();
                out.push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!(
                        "duplicate key `{name}` in object literal — later values override earlier ones"
                    ),
                    ..Default::default()
                });
            } else {
                seen.insert(name, key_span);
            }
        }
    });
    out
}

/// Pull out identifier-or-string keys at depth 1 of the object
/// literal starting at `slice`. `slice_base` is the byte offset of
/// `slice[0]` in the overall source, so returned spans are absolute.
/// Skips nested braces/brackets/parens, strings, heredocs, and
/// comments so nested object keys don't leak into the top-level set.
fn extract_top_level_keys(slice: &str, slice_base: usize) -> Vec<(String, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let bytes = slice.as_bytes();
    if !bytes.starts_with(b"{") {
        return out;
    }
    let mut depth: i32 = 0;
    let mut i = 0;
    let mut at_key_position = false;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'{' => {
                depth += 1;
                if depth == 1 {
                    at_key_position = true;
                }
                i += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                i += 1;
            }
            b'(' | b'[' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' => {
                depth -= 1;
                i += 1;
            }
            b',' | b'\n' if depth == 1 => {
                at_key_position = true;
                i += 1;
            }
            b'#' => {
                // Line comment to end-of-line.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'"' if depth == 1 && at_key_position => {
                let (end, content) = match read_string(bytes, i) {
                    Some(v) => v,
                    None => break,
                };
                // After the string, skip whitespace and look for `=`.
                let mut j = end;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    out.push((content, slice_base + i..slice_base + end));
                    at_key_position = false;
                }
                i = j;
            }
            b'"' => {
                // String value — skip it.
                let (end, _) = match read_string(bytes, i) {
                    Some(v) => v,
                    None => break,
                };
                i = end;
            }
            _ if is_ident_start(b) && depth == 1 && at_key_position => {
                let start = i;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                let ident = &slice[start..i];
                let mut j = i;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    out.push((
                        ident.to_string(),
                        slice_base + start..slice_base + i,
                    ));
                    at_key_position = false;
                }
                i = j;
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
            }
            _ => {
                at_key_position = false;
                i += 1;
            }
        }
    }
    out
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Consume a double-quoted string starting at `bytes[i] == b'"'`.
/// Returns the byte offset after the closing quote and the string
/// contents (with escape sequences resolved only for `\"` / `\\`).
fn read_string(bytes: &[u8], i: usize) -> Option<(usize, String)> {
    debug_assert_eq!(bytes[i], b'"');
    let mut j = i + 1;
    let mut contents = String::new();
    while j < bytes.len() {
        match bytes[j] {
            b'\\' if j + 1 < bytes.len() => {
                match bytes[j + 1] {
                    b'"' => contents.push('"'),
                    b'\\' => contents.push('\\'),
                    b'n' => contents.push('\n'),
                    b't' => contents.push('\t'),
                    other => contents.push(other as char),
                }
                j += 2;
            }
            b'"' => return Some((j + 1, contents)),
            other => {
                contents.push(other as char);
                j += 1;
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        map_duplicate_keys_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_duplicate_string_key() {
        let d = diags(r#"locals { x = { "a" = 1, "a" = 2 } }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("`a`"), "got: {}", d[0].message);
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn flags_duplicate_ident_key() {
        let d = diags(r#"locals { x = { a = 1, a = 2 } }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn flags_mixed_ident_and_string_duplicate() {
        let d = diags(r#"locals { x = { a = 1, "a" = 2 } }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn silent_for_unique_keys() {
        let d = diags(r#"locals { x = { a = 1, b = 2, c = 3 } }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_dynamic_keys() {
        // Can't resolve `(var.x)` statically — skip it instead of
        // false-flagging.
        let d = diags(r#"locals { x = { (var.a) = 1, (var.b) = 2 } }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_duplicates_in_nested_object() {
        let d = diags(r#"locals { x = { outer = { k = 1, k = 2 } } }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn same_name_at_different_nesting_is_fine() {
        let d = diags(r#"locals { x = { a = { a = 1 } } }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }
}
