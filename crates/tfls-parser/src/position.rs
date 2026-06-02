//! Position conversions between LSP coordinates and byte offsets.
//!
//! LSP uses 0-indexed (line, character) positions where `character` is a
//! UTF-16 code-unit offset. We convert via `ropey`, which tracks UTF-8 and
//! line breaks efficiently. For now we treat `character` as a UTF-8 byte
//! offset within the line; a follow-up will handle UTF-16 properly once
//! LSP clients request it via `positionEncodings`.

use lsp_types::{Position, Range};
use ropey::Rope;

use crate::error::ParseError;

/// Convert an LSP `Position` to an absolute byte offset in the rope.
///
/// Per LSP 3.17 §Position: "If the character value is greater than the
/// line length it defaults back to the line length." We therefore clamp
/// `pos.character` to the line's visible length (excluding any trailing
/// `\n` / `\r\n`). Clients routinely send past-EOL positions — e.g.
/// when the cursor is at column N on an auto-indented blank line that
/// the server still sees as empty because a didChange hasn't landed —
/// and dropping those as errors produces silent empty completion.
pub fn lsp_position_to_byte_offset(rope: &Rope, pos: Position) -> Result<usize, ParseError> {
    let line_idx = pos.line as usize;
    let total_lines = rope.len_lines();
    if line_idx >= total_lines {
        return Err(ParseError::LineOutOfBounds {
            line: pos.line,
            total_lines,
        });
    }

    let line_start_byte = rope.line_to_byte(line_idx);
    let line = rope.line(line_idx);

    // LSP `Position.character` is a UTF-16 code-unit offset within the
    // line (the default encoding; we don't negotiate `positionEncoding`).
    // It must NOT be treated as a byte offset — on a line with multibyte
    // text before the cursor that both mislocates the cursor and can land
    // mid-codepoint, which panics callers that slice `&source[..offset]`.
    //
    // Visible char count excludes any trailing newline so a clamped
    // past-EOL column stays on the requested line.
    let visible_chars = {
        let mut n = line.len_chars();
        if n > 0 && line.char(n - 1) == '\n' {
            n -= 1;
            if n > 0 && line.char(n - 1) == '\r' {
                n -= 1;
            }
        } else if n > 0 && line.char(n - 1) == '\r' {
            n -= 1;
        }
        n
    };
    let visible_cu = line.slice(..visible_chars).len_utf16_cu();
    let cu = (pos.character as usize).min(visible_cu);
    let char_in_line = line.utf16_cu_to_char(cu);
    let byte_in_line = line.char_to_byte(char_in_line);
    Ok(line_start_byte + byte_in_line)
}

/// Convert a byte offset to an LSP `Position`.
pub fn byte_offset_to_lsp_position(rope: &Rope, offset: usize) -> Result<Position, ParseError> {
    let total_bytes = rope.len_bytes();
    if offset > total_bytes {
        return Err(ParseError::ByteOffsetOutOfBounds {
            offset,
            length: total_bytes,
        });
    }

    let line_idx = rope.byte_to_line(offset);
    let line_start_byte = rope.line_to_byte(line_idx);
    // Emit a UTF-16 code-unit column (LSP's default `Position.character`
    // encoding), not a byte difference — they diverge on multibyte lines.
    let char_in_line = rope.byte_to_char(offset) - rope.byte_to_char(line_start_byte);
    let character = rope.line(line_idx).slice(..char_in_line).len_utf16_cu();

    Ok(Position {
        line: line_idx as u32,
        character: character as u32,
    })
}

/// Convert an hcl-edit span (byte range) to an LSP `Range`.
pub fn hcl_span_to_lsp_range(
    rope: &Rope,
    span: std::ops::Range<usize>,
) -> Result<Range, ParseError> {
    let start = byte_offset_to_lsp_position(rope, span.start)?;
    let end = byte_offset_to_lsp_position(rope, span.end)?;
    Ok(Range { start, end })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn position_at_start_of_empty_doc_is_zero() {
        let r = rope("");
        let offset = lsp_position_to_byte_offset(&r, Position::new(0, 0))
            .expect("start of empty doc");
        assert_eq!(offset, 0);
    }

    #[test]
    fn position_round_trips_in_single_line() {
        let r = rope("hello world");
        for col in 0..=11 {
            let pos = Position::new(0, col);
            let offset = lsp_position_to_byte_offset(&r, pos).expect("valid position");
            let back = byte_offset_to_lsp_position(&r, offset).expect("valid offset");
            assert_eq!(back, pos, "col={col}");
        }
    }

    #[test]
    fn position_round_trips_across_lines() {
        let r = rope("line one\nline two\nline three");
        let positions = [
            Position::new(0, 0),
            Position::new(0, 4),
            Position::new(1, 0),
            Position::new(1, 8),
            Position::new(2, 10),
        ];
        for pos in positions {
            let offset = lsp_position_to_byte_offset(&r, pos).expect("valid");
            let back = byte_offset_to_lsp_position(&r, offset).expect("valid");
            assert_eq!(back, pos);
        }
    }

    #[test]
    fn utf16_column_maps_to_byte_offset_after_multibyte() {
        // `café = ` — `é` is 2 bytes / 1 UTF-16 CU. Column 5 (UTF-16) is
        // right after `café ` (the space), byte offset 6.
        let r = rope("café = 1\n");
        let off = lsp_position_to_byte_offset(&r, Position::new(0, 5)).expect("valid");
        assert_eq!(off, 6, "UTF-16 col 5 → byte 6 (é is 2 bytes)");
        // Round-trips back to the same UTF-16 column.
        let back = byte_offset_to_lsp_position(&r, off).expect("valid");
        assert_eq!(back, Position::new(0, 5));
    }

    #[test]
    fn utf16_column_handles_astral_surrogate_pair() {
        // `😀` is 4 bytes / 2 UTF-16 code units. Column 2 is right after it.
        let r = rope("😀x\n");
        let off = lsp_position_to_byte_offset(&r, Position::new(0, 2)).expect("valid");
        assert_eq!(off, 4, "after the 4-byte emoji");
        assert_eq!(
            byte_offset_to_lsp_position(&r, off).expect("valid"),
            Position::new(0, 2)
        );
    }

    #[test]
    fn past_eol_column_clamps_without_panic() {
        let r = rope("café\n");
        // Column 99 past EOL clamps to end of the visible line (byte 5).
        let off = lsp_position_to_byte_offset(&r, Position::new(0, 99)).expect("valid");
        assert_eq!(off, 5);
    }

    #[test]
    fn position_rejects_line_out_of_bounds() {
        let r = rope("one line only");
        let err = lsp_position_to_byte_offset(&r, Position::new(10, 0));
        assert!(matches!(err, Err(ParseError::LineOutOfBounds { .. })));
    }

    #[test]
    fn position_clamps_past_eol_character_to_visible_line_length() {
        // Past-EOL clamp on a blank line — client may send character > 0
        // when the server still sees an empty line (autoindent races,
        // stale didChange). Must not drop the request.
        let r = rope("resource \"x\" {\n\n}\n");
        let offset = lsp_position_to_byte_offset(&r, Position::new(1, 2))
            .expect("past-EOL on blank line clamps rather than errors");
        // Clamped character=0 on blank line 1 → start of that line.
        assert_eq!(offset, r.line_to_byte(1));
        let back = byte_offset_to_lsp_position(&r, offset).expect("valid offset");
        assert_eq!(back, Position::new(1, 0));
    }

    #[test]
    fn position_clamps_past_eol_on_content_line() {
        // character > visible length on a non-blank line clamps to the
        // visible length (excluding trailing newline).
        let r = rope("hello\nworld\n");
        let offset = lsp_position_to_byte_offset(&r, Position::new(0, 100))
            .expect("past-EOL on content line clamps");
        assert_eq!(offset, r.line_to_byte(0) + 5);
    }

    #[test]
    fn position_clamps_past_eol_with_crlf() {
        let r = rope("one\r\ntwo\r\n");
        let offset = lsp_position_to_byte_offset(&r, Position::new(0, 50))
            .expect("CRLF visible length");
        assert_eq!(offset, r.line_to_byte(0) + 3);
    }

    #[test]
    fn byte_offset_rejects_out_of_bounds() {
        let r = rope("abc");
        let err = byte_offset_to_lsp_position(&r, 999);
        assert!(matches!(err, Err(ParseError::ByteOffsetOutOfBounds { .. })));
    }

    #[test]
    fn span_converts_to_range() {
        let r = rope("hello\nworld");
        let range = hcl_span_to_lsp_range(&r, 0..5).expect("valid span");
        assert_eq!(range.start, Position::new(0, 0));
        assert_eq!(range.end, Position::new(0, 5));
    }
}
