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
    let line_byte_len = line.len_bytes();

    // Exclude any trailing newline so the clamped position stays on
    // the requested line instead of landing at the start of the next.
    let visible_line_len = {
        let mut len = line_byte_len;
        if len > 0 {
            let last = rope.byte(line_start_byte + len - 1);
            if last == b'\n' {
                len -= 1;
                // Strip the carriage return of a CRLF sequence.
                if len > 0 && rope.byte(line_start_byte + len - 1) == b'\r' {
                    len -= 1;
                }
            } else if last == b'\r' {
                len -= 1;
            }
        }
        len
    };

    let char_offset = (pos.character as usize).min(visible_line_len);
    Ok(line_start_byte + char_offset)
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
    let character = offset.saturating_sub(line_start_byte);

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
