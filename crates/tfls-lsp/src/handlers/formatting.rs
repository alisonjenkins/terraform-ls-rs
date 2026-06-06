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
    let Some(doc) = backend.state.documents.get(&uri) else {
        tracing::info!(uri = %uri, "formatting: document not in state");
        return Ok(None);
    };

    let text = doc.rope.to_string();
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

    tracing::info!(
        in_bytes = text.len(),
        out_bytes = formatted.len(),
        "formatting: emitting edit"
    );
    Ok(Some(vec![TextEdit {
        range: whole_document_range(&doc.rope),
        new_text: formatted,
    }]))
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
    let last_line = rope.len_lines().saturating_sub(1) as u32;
    let last_line_len = rope
        .get_line(last_line as usize)
        .map(|l| l.len_chars() as u32)
        .unwrap_or(0);
    Range {
        start: Position::new(0, 0),
        end: Position::new(last_line, last_line_len),
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

    #[test]
    fn match_trailing_newline_respects_slice() {
        assert_eq!(match_trailing_newline("a\n".into(), "a"), "a");
        assert_eq!(match_trailing_newline("a\n".into(), "a\n"), "a\n");
    }
}
