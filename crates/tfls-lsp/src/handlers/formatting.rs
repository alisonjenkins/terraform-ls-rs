//! `textDocument/formatting`, `textDocument/rangeFormatting`,
//! `textDocument/onTypeFormatting`.

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{
    DocumentFormattingParams, DocumentOnTypeFormattingParams, DocumentRangeFormattingParams,
    Position, Range, TextEdit,
};
use ropey::Rope;
use tfls_format::format_source;
use tfls_parser::{
    byte_offset_to_lsp_position, hcl_span_to_lsp_range, lsp_position_to_byte_offset,
};
use tower_lsp_server::jsonrpc;

use crate::backend::Backend;

pub async fn formatting(
    backend: &Backend,
    params: DocumentFormattingParams,
) -> jsonrpc::Result<Option<Vec<TextEdit>>> {
    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document.uri) else {
        return Ok(None);
    };
    let style = backend.state.config.snapshot().format_style;
    tracing::info!(uri = %uri, ?style, "formatting: invocation");

    // Serialize against any in-flight `did_change` apply for this document
    // so we never read a rope that's mid-update (which would format stale
    // text). See `Backend::doc_lock`.
    let lock = backend.doc_lock(&uri);
    let _guard = lock.lock().await;

    let text = {
        let Some(doc) = backend.state.documents.get(&uri) else {
            tracing::info!(uri = %uri, "formatting: document not in state");
            return Ok(None);
        };
        doc.rope.to_string()
    }; // drop the DashMap read ref before the CPU-heavy format below

    let formatted = match format_source(&text, style) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(error = %e, "formatting: backend rejected source");
            return Ok(None);
        }
    };

    if formatted == text {
        tracing::info!("formatting: no-op (already formatted)");
        return Ok(Some(Vec::new()));
    }

    // Emit a MINIMAL line-level diff, not a whole-document replace. A
    // whole-doc replace overwrites every line — so if the formatter ran on
    // even slightly-stale text it clobbers the user's just-typed edits
    // (the reported "formatting reverts my changes" bug). A minimal diff
    // touches only the lines the formatter actually changed, so lines the
    // formatter left alone (e.g. a freshly-edited `source = "..."`) survive.
    let edits = minimal_text_edits(&text, &formatted);
    tracing::info!(
        in_bytes = text.len(),
        out_bytes = formatted.len(),
        edits = edits.len(),
        "formatting: emitting minimal edits"
    );
    Ok(Some(edits))
}

/// Line-level minimal diff from `old` to `new` as a set of `TextEdit`s,
/// one per changed hunk. Unchanged lines are never touched. Ranges are
/// whole-line spans (`line, 0`)..(`line, 0`), EOF-clamped via the rope.
fn minimal_text_edits(old: &str, new: &str) -> Vec<TextEdit> {
    use similar::{DiffOp, TextDiff};

    let rope = Rope::from_str(old);
    let diff = TextDiff::from_lines(old, new);
    let new_lines: Vec<&str> = diff.iter_new_slices().collect();

    let line_pos = |line: usize| -> Position {
        let byte = if line >= rope.len_lines() {
            rope.len_bytes()
        } else {
            rope.line_to_byte(line)
        };
        byte_offset_to_lsp_position(&rope, byte).unwrap_or_default()
    };
    let take_new = |start: usize, len: usize| -> String {
        new_lines
            .get(start..start + len)
            .map(|s| s.concat())
            .unwrap_or_default()
    };

    let mut edits = Vec::new();
    for op in diff.ops() {
        let (old_start, old_end, new_text) = match *op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete {
                old_index, old_len, ..
            } => (old_index, old_index + old_len, String::new()),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => (old_index, old_index, take_new(new_index, new_len)),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => (
                old_index,
                old_index + old_len,
                take_new(new_index, new_len),
            ),
        };
        edits.push(TextEdit {
            range: Range {
                start: line_pos(old_start),
                end: line_pos(old_end),
            },
            new_text,
        });
    }
    edits
}

/// `textDocument/rangeFormatting` — format only the given range.
///
/// The sliced text must parse as a standalone HCL body, so attempts
/// like selecting an attribute mid-block are rejected (returns
/// `None`) rather than corrupting the document.
pub async fn range_formatting(
    backend: &Backend,
    params: DocumentRangeFormattingParams,
) -> jsonrpc::Result<Option<Vec<TextEdit>>> {
    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document.uri) else {
        return Ok(None);
    };
    // Serialize against an in-flight did_change apply (see Backend::doc_lock).
    let lock = backend.doc_lock(&uri);
    let _guard = lock.lock().await;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };

    let Some(slice) = slice_text(&doc.rope, params.range) else {
        return Ok(None);
    };
    let style = backend.state.config.snapshot().format_style;
    let Ok(formatted) = format_source(&slice, style) else {
        return Ok(None);
    };
    let formatted = match_trailing_newline(formatted, &slice);
    if formatted == slice {
        return Ok(Some(Vec::new()));
    }

    Ok(Some(vec![TextEdit {
        range: params.range,
        new_text: formatted,
    }]))
}

/// `format_source` always appends a trailing newline, but a partial-range
/// slice (an enclosing block or a user selection) does not include the
/// document's newline that follows the range — emitting the formatter's
/// trailing newline would then insert a spurious blank line after the range.
/// Match the slice's trailing-newline-ness.
fn match_trailing_newline(formatted: String, slice: &str) -> String {
    if slice.ends_with('\n') {
        formatted
    } else {
        formatted.trim_end_matches('\n').to_string()
    }
}

/// `textDocument/onTypeFormatting` — triggered after typing `}`
/// (close of an enclosing block) or `=` (assignment, where
/// alignment fires across the surrounding run of single-line
/// attributes). Either way, we reformat the smallest enclosing
/// block; tf-format's `=` alignment then re-aligns the column
/// across that block's attribute run.
pub async fn on_type_formatting(
    backend: &Backend,
    params: DocumentOnTypeFormattingParams,
) -> jsonrpc::Result<Option<Vec<TextEdit>>> {
    if !matches!(params.ch.as_str(), "}" | "=") {
        return Ok(None);
    }

    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document_position.text_document.uri)
    else {
        return Ok(None);
    };
    let pos = params.text_document_position.position;
    // Serialize against an in-flight did_change apply (see Backend::doc_lock).
    let lock = backend.doc_lock(&uri);
    let _guard = lock.lock().await;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let Some(body) = doc.parsed.body.as_ref() else {
        return Ok(None);
    };

    let Some(range) = enclosing_block_range(body, &doc.rope, pos) else {
        return Ok(None);
    };
    let Some(slice) = slice_text(&doc.rope, range) else {
        return Ok(None);
    };
    let style = backend.state.config.snapshot().format_style;
    let Ok(formatted) = format_source(&slice, style) else {
        return Ok(None);
    };
    let formatted = match_trailing_newline(formatted, &slice);
    if formatted == slice {
        return Ok(Some(Vec::new()));
    }

    Ok(Some(vec![TextEdit {
        range,
        new_text: formatted,
    }]))
}

pub(super) fn whole_document_range(rope: &Rope) -> Range {
    // The end must be a UTF-16 code-unit column to match the server's default
    // positionEncoding (every other position the server emits goes through
    // `byte_offset_to_lsp_position`, which uses `len_utf16_cu()`). Using
    // `len_chars()` undercounts on a non-newline-terminated final line that
    // contains a non-BMP scalar (an emoji is 1 char but 2 UTF-16 units), so a
    // whole-document replace would leave the trailing UTF-16 unit(s) appended
    // after the formatted text — silent buffer corruption. Anchoring the end
    // at the rope's last byte offset reuses the exact same conversion.
    let end = byte_offset_to_lsp_position(rope, rope.len_bytes())
        .unwrap_or_else(|_| Position::new(rope.len_lines().saturating_sub(1) as u32, 0));
    Range {
        start: Position::new(0, 0),
        end,
    }
}

pub(super) fn slice_text(rope: &Rope, range: Range) -> Option<String> {
    let start = lsp_position_to_byte_offset(rope, range.start).ok()?;
    let end = lsp_position_to_byte_offset(rope, range.end).ok()?;
    if end < start {
        return None;
    }
    let sc = rope.byte_to_char(start);
    let ec = rope.byte_to_char(end);
    Some(rope.slice(sc..ec).to_string())
}

fn enclosing_block_range(body: &Body, rope: &Rope, pos: Position) -> Option<Range> {
    let mut best: Option<Range> = None;
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        let Some(span) = block.span() else { continue };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        if contains_or_touches(&range, pos) {
            // Prefer the deepest (shortest) match.
            best = match best {
                Some(b) if area(&b) <= area(&range) => Some(b),
                _ => Some(range),
            };
            // Descend into nested blocks for a tighter match.
            if let Some(inner) = enclosing_block_range(&block.body, rope, pos) {
                best = Some(inner);
            }
        }
    }
    best
}

fn contains_or_touches(range: &Range, pos: Position) -> bool {
    let after_start = (pos.line, pos.character) >= (range.start.line, range.start.character);
    // Note: `<=` includes the position immediately after the closing `}`.
    let before_end = (pos.line, pos.character) <= (range.end.line, range.end.character);
    after_start && before_end
}

fn area(r: &Range) -> u64 {
    let line_span = (r.end.line - r.start.line) as u64;
    line_span * 10_000 + (r.end.character as u64).saturating_sub(r.start.character as u64)
}

// Used transitively via position conversions — keeps the import tree clean.
#[allow(dead_code)]
fn _byte_noop() {
    let _ = byte_offset_to_lsp_position;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::Position;
    use tfls_state::FormatStyle;

    // Regression: on-type / range formatting must not append a trailing
    // newline when the formatted range ends at `}` (no newline) — doing so
    // inserts a spurious blank line after the block.
    #[test]
    fn enclosing_block_format_keeps_no_trailing_newline() {
        let src = "variable \"v\" {\n  type = list(string)\n  description = \"x\"\n}\n";
        let body = hcl_edit::parser::parse_body(src).expect("parse");
        let rope = Rope::from_str(src);
        let range = enclosing_block_range(&body, &rope, Position::new(1, 7)).expect("range");
        let slice = slice_text(&rope, range).expect("slice");
        assert!(
            !slice.ends_with('\n'),
            "block slice ends at }} (no newline)"
        );
        let formatted = format_source(&slice, FormatStyle::Minimal).expect("fmt");
        let edit = match_trailing_newline(formatted, &slice);
        assert!(
            !edit.ends_with('\n'),
            "formatted block must not end with a newline; got:\n{edit:?}"
        );
        assert!(edit.contains("type        = list(string)"));
    }

    // Regression (MED-5): the whole-document range end column must be a
    // UTF-16 code-unit count, not a `char` count. On a final line that is NOT
    // newline-terminated and holds a non-BMP scalar (emoji), `len_chars()`
    // undercounts vs UTF-16 (🎉 is 1 char but 2 UTF-16 units). An undercounted
    // end leaves trailing UTF-16 unit(s) un-replaced by a whole-document edit.
    #[test]
    fn whole_document_range_end_is_utf16_on_non_bmp_final_line() {
        let src = "resource \"x\" \"y\" {}\n# 🎉 done";
        let rope = Rope::from_str(src);
        let range = whole_document_range(&rope);
        assert_eq!(range.start, Position::new(0, 0));

        // Final line "# 🎉 done": 8 chars, but 🎉 is 2 UTF-16 units → 9 units.
        let last_line = rope.len_lines().saturating_sub(1);
        let last_line_slice = rope.line(last_line);
        let utf16_len = last_line_slice.len_utf16_cu() as u32;
        let char_len = last_line_slice.len_chars() as u32;
        assert_eq!(utf16_len, 9, "🎉 counts as 2 UTF-16 units");
        assert_eq!(char_len, 8, "🎉 counts as 1 char");

        assert_eq!(
            range.end,
            Position::new(last_line as u32, utf16_len),
            "end column must be UTF-16 code units, not char count"
        );
    }

    #[test]
    fn match_trailing_newline_respects_slice() {
        assert_eq!(match_trailing_newline("a\n".into(), "a"), "a");
        assert_eq!(match_trailing_newline("a\n".into(), "a\n"), "a\n");
    }

    fn apply_edits(text: &str, mut edits: Vec<TextEdit>) -> String {
        let rope = Rope::from_str(text);
        let byte_of = |p: Position| -> usize {
            lsp_position_to_byte_offset(&rope, p).unwrap_or(rope.len_bytes())
        };
        // Apply in reverse document order so earlier byte offsets (computed
        // against the original rope) stay valid as we mutate.
        edits.sort_by(|a, b| {
            (b.range.start.line, b.range.start.character)
                .cmp(&(a.range.start.line, a.range.start.character))
        });
        let mut s = text.to_string();
        for e in edits {
            s.replace_range(byte_of(e.range.start)..byte_of(e.range.end), &e.new_text);
        }
        s
    }

    #[test]
    fn minimal_edits_roundtrip_to_new() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nD\n";
        let edits = minimal_text_edits(old, new);
        assert_eq!(apply_edits(old, edits), new);
    }

    #[test]
    fn minimal_edits_leave_unchanged_lines_untouched() {
        // Lines 0 and 2 are unchanged; only line 1 differs. No edit range may
        // cover an unchanged line — this is what stops a stale format from
        // reverting a line the formatter didn't touch.
        let old = "keep_a\nchange_me\nkeep_b\n";
        let new = "keep_a\nCHANGED\nkeep_b\n";
        let edits = minimal_text_edits(old, new);
        assert!(!edits.is_empty());
        for e in &edits {
            // No edit may start before line 1 or end after line 2.
            assert!(
                e.range.start.line >= 1 && e.range.end.line <= 2,
                "edit must be confined to the changed line; got {:?}",
                e.range
            );
        }
        assert_eq!(apply_edits(old, edits), new);
    }

    #[test]
    fn minimal_edits_preserve_a_value_the_formatter_keeps() {
        // The reported bug shape: a `source` value the user just set. As long
        // as it is identical in old+new (formatter preserves values), the
        // minimal diff must not alter it.
        let old = "module \"m\" {\n  source = \"../new-path\"\n  x = 1\n}\n";
        let new = "module \"m\" {\n  source = \"../new-path\"\n  y = 2\n}\n";
        let out = apply_edits(old, minimal_text_edits(old, new));
        assert!(out.contains("\"../new-path\""), "source value preserved: {out}");
        assert_eq!(out, new);
    }
}
