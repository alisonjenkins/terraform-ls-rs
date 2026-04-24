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
            b'<' if bytes.get(i + 1) == Some(&b'<') => {
                // Heredoc opener: `<<IDENT\n…body…\nIDENT` or
                // `<<-IDENT\n…body…\n  IDENT`. Skip the body so
                // `ident = …` lines inside it don't leak as phantom
                // keys and so brace imbalance in the body (e.g.
                // `function foo() {`) doesn't desync `depth`.
                //
                // Falls through to the catch-all when the shape
                // doesn't match a real heredoc — so `a << b` style
                // bit-shift uses (if anyone wrote them) don't
                // regress.
                match skip_heredoc(bytes, i) {
                    Some(end) => {
                        // Terminator line always ends the heredoc;
                        // its trailing `\n` IS the separator between
                        // object entries, so the next byte is at key
                        // position.
                        i = end;
                        at_key_position = true;
                    }
                    None => {
                        at_key_position = false;
                        i += 1;
                    }
                }
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
///
/// Template interpolations (`${…}`) and template directives (`%{…}`)
/// are handled specially: the reader tracks a per-sigil depth and
/// descends into nested strings inside them. Without this, an inner
/// `"` inside `${…}` would be misread as the outer string's
/// terminator — the tokenizer then resumed mid-expression and
/// mis-classified bytes such as `}` as object keys, producing
/// spurious duplicate-key diagnostics.
fn read_string(bytes: &[u8], i: usize) -> Option<(usize, String)> {
    debug_assert_eq!(bytes[i], b'"');
    let mut j = i + 1;
    let mut contents = String::new();
    let mut interp_depth: u32 = 0;
    while j < bytes.len() {
        // Escape sequences take priority over everything else.
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            match bytes[j + 1] {
                b'"' => contents.push('"'),
                b'\\' => contents.push('\\'),
                b'n' => contents.push('\n'),
                b't' => contents.push('\t'),
                other => contents.push(other as char),
            }
            j += 2;
            continue;
        }

        // Literal `${` / `%{` — the doubled sigil is a literal
        // dollar / percent followed by `{`, NOT an interpolation
        // opener. Keep one sigil + one `{` in the contents; don't
        // bump interp depth.
        if j + 2 < bytes.len()
            && (bytes[j] == b'$' || bytes[j] == b'%')
            && bytes[j + 1] == bytes[j]
            && bytes[j + 2] == b'{'
        {
            contents.push(bytes[j] as char);
            contents.push(b'{' as char);
            j += 3;
            continue;
        }

        // Interpolation or directive opener: `${` or `%{`.
        if j + 1 < bytes.len() && (bytes[j] == b'$' || bytes[j] == b'%') && bytes[j + 1] == b'{' {
            contents.push(bytes[j] as char);
            contents.push(b'{' as char);
            j += 2;
            interp_depth += 1;
            continue;
        }

        if interp_depth > 0 {
            match bytes[j] {
                b'{' => {
                    interp_depth += 1;
                    contents.push('{');
                    j += 1;
                }
                b'}' => {
                    interp_depth -= 1;
                    contents.push('}');
                    j += 1;
                }
                b'"' => {
                    // Nested string INSIDE an interpolation — recurse
                    // so the outer string's `"` terminator check
                    // doesn't fire on the inner string's bytes.
                    let (end, _) = read_string(bytes, j)?;
                    for &b in &bytes[j..end] {
                        contents.push(b as char);
                    }
                    j = end;
                }
                other => {
                    contents.push(other as char);
                    j += 1;
                }
            }
            continue;
        }

        if bytes[j] == b'"' {
            return Some((j + 1, contents));
        }

        contents.push(bytes[j] as char);
        j += 1;
    }
    None
}

/// Skip a heredoc starting at `bytes[i] == b'<'` (with
/// `bytes[i+1] == b'<'`). Returns the byte offset after the
/// terminator line, or `None` if the shape doesn't match a real
/// heredoc (caller falls back to single-byte advance).
///
/// Recognises `<<IDENT\n` and `<<-IDENT\n`. The terminator line
/// matches `^[ \t]*IDENT[ \t]*(\r?\n|$)`.
fn skip_heredoc(bytes: &[u8], i: usize) -> Option<usize> {
    debug_assert!(bytes.get(i) == Some(&b'<') && bytes.get(i + 1) == Some(&b'<'));

    // Parse optional `-` + identifier.
    let mut k = i + 2;
    if bytes.get(k) == Some(&b'-') {
        k += 1;
    }
    let ident_start = k;
    while k < bytes.len() && is_ident_continue(bytes[k]) {
        k += 1;
    }
    if k == ident_start {
        // No identifier — not a heredoc.
        return None;
    }
    let terminator = &bytes[ident_start..k];

    // Consume anything on the rest of the opener line, require a
    // newline. HCL heredocs require the body to start on the next
    // line; if we don't see a newline, bail.
    while k < bytes.len() && bytes[k] != b'\n' {
        // Only whitespace is valid between IDENT and the newline.
        // Any other byte means this wasn't a heredoc shape after
        // all — e.g. `a << b + 1` doesn't have a real ident, and
        // if an ident sneaks through, further characters mean it
        // isn't a heredoc marker.
        if !matches!(bytes[k], b' ' | b'\t' | b'\r') {
            return None;
        }
        k += 1;
    }
    if k == bytes.len() {
        // Malformed: `<<EOT` at end of file with no body.
        return None;
    }
    // Skip the newline.
    k += 1;

    // Scan lines for the terminator.
    loop {
        let line_start = k;
        // Find end of line.
        let mut line_end = line_start;
        while line_end < bytes.len() && bytes[line_end] != b'\n' {
            line_end += 1;
        }

        // Strip leading whitespace.
        let mut trim_start = line_start;
        while trim_start < line_end && matches!(bytes[trim_start], b' ' | b'\t') {
            trim_start += 1;
        }
        // Strip trailing whitespace (including CR).
        let mut trim_end = line_end;
        while trim_end > trim_start
            && matches!(bytes[trim_end - 1], b' ' | b'\t' | b'\r')
        {
            trim_end -= 1;
        }

        if &bytes[trim_start..trim_end] == terminator {
            // Found it. Advance past the newline if present.
            return Some(if line_end < bytes.len() {
                line_end + 1
            } else {
                line_end
            });
        }

        if line_end >= bytes.len() {
            // End of slice reached without terminator — bail so
            // the caller can at least advance by one byte rather
            // than looping.
            return None;
        }
        k = line_end + 1;
    }
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

    // --- heredoc handling -------------------------------------------
    //
    // The tokenizer scans raw source text. Heredoc bodies contain
    // arbitrary lines, some of which look like `ident = value`. Before
    // the heredoc arm was added, every such line was emitted as a key
    // of the ENCLOSING object, producing bogus duplicate warnings
    // whenever two sibling heredoc values shared the same line shape.

    #[test]
    fn heredoc_body_assignment_lines_do_not_trigger_duplicates() {
        let src = "locals {\n  x = {\n    a = <<-EOT\n      name = \"x\"\n      value = \"y\"\n    EOT\n    b = <<-EOT\n      name = \"x\"\n      value = \"z\"\n    EOT\n  }\n}\n";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "heredoc body leaked phantom keys: {d:?}"
        );
    }

    #[test]
    fn heredoc_with_template_directive_in_body_does_not_trigger() {
        let src = "locals {\n  x = {\n    a = <<-EOT\n      %{ for i in [] }item_name = ${i}%{ endfor }\n    EOT\n    b = <<-EOT\n      %{ for i in [] }item_name = ${i}%{ endfor }\n    EOT\n  }\n}\n";
        let d = diags(src);
        assert!(d.is_empty(), "template directive in heredoc leaked: {d:?}");
    }

    #[test]
    fn heredoc_with_unbalanced_braces_does_not_desync_depth() {
        // The heredoc body has a dangling `{`. Without proper
        // skipping, depth would go to 2 and stay there — the outer
        // object's closing `}` would never drop depth to 0 and later
        // real duplicates would be missed. Add a genuine `name` dup
        // AFTER the heredocs and assert it still fires.
        let src = "locals {\n  x = {\n    a = <<-EOT\n      function foo() {\n    EOT\n    name = 1\n    name = 2\n  }\n}\n";
        let d = diags(src);
        assert_eq!(d.len(), 1, "expected one real dup, got: {d:?}");
        assert!(d[0].message.contains("`name`"), "got: {}", d[0].message);
    }

    #[test]
    fn indented_heredoc_terminator_allows_leading_whitespace() {
        let src = "locals {\n  x = {\n    a = <<-EOT\n      hi\n    EOT\n    b = <<-EOT\n      hi\n    EOT\n  }\n}\n";
        let d = diags(src);
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn unindented_heredoc_marker_also_handled() {
        let src = "locals {\n  x = {\n    a = <<EOT\nname = 1\nEOT\n    b = <<EOT\nname = 2\nEOT\n  }\n}\n";
        let d = diags(src);
        assert!(d.is_empty(), "{d:?}");
    }

    // --- interpolation-aware string reader --------------------------
    //
    // HCL template interpolations can contain nested string literals:
    // `"pre${"a"}"`. Without interp-depth tracking, the reader
    // terminates the outer string at the first inner `"`, so the
    // tokenizer resumes mid-expression and mis-classifies subsequent
    // bytes (e.g. the `}` closing the interpolation) as keys.

    #[test]
    fn string_with_interpolation_containing_nested_strings() {
        let src = "locals { x = { \"pre${\"a\"}\" = 1, \"pre${\"b\"}\" = 2 } }";
        let d = diags(src);
        assert!(
            d.is_empty(),
            "interp with nested string produced bogus dup: {d:?}"
        );
    }

    #[test]
    fn string_with_dollar_dollar_escape_does_not_enter_interp() {
        // `$${name}` is the literal string `${name}`. The key reader
        // must NOT enter interp mode and must close the outer string
        // normally.
        let src = "locals { x = { a = \"literal $${name}\", b = \"literal $${name}\" } }";
        let d = diags(src);
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn string_with_template_directive_containing_nested_string() {
        let src = "locals { x = { a = \"%{ if \"x\" == \"x\" }yes%{ endif }\", b = \"%{ if \"x\" == \"x\" }yes%{ endif }\" } }";
        let d = diags(src);
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn genuine_duplicate_after_tricky_string_still_fires() {
        let src = "locals { x = { a = \"pre${\"x\"}\", name = 1, name = 2 } }";
        let d = diags(src);
        assert_eq!(d.len(), 1, "expected 1 real dup, got: {d:?}");
        assert!(d[0].message.contains("`name`"), "got: {}", d[0].message);
    }

    #[test]
    fn genuine_duplicate_after_heredoc_still_fires() {
        let src = "locals {\n  x = {\n    a = <<-EOT\n      plain body\n    EOT\n    name = 1\n    name = 2\n  }\n}\n";
        let d = diags(src);
        assert_eq!(d.len(), 1, "{d:?}");
    }
}
