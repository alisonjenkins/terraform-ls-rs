//! Position conversions between LSP coordinates and byte offsets.
//!
//! LSP uses 0-indexed (line, character) positions where `character` is a
//! UTF-16 code-unit offset. We track UTF-8 / UTF-16 via `ropey`, but we
//! compute *line boundaries ourselves* by scanning for `\n` only.
//!
//! Why not lean on `rope.line()` / `rope.len_lines()`? `ropey` is pulled
//! with its default `unicode_lines` feature, under which it treats U+000B
//! (VT), U+000C (FF), U+0085 (NEL), U+2028 (LS), U+2029 (PS) and a lone
//! U+000D (CR) as line breaks. LSP clients (VS Code, Neovim, …) split lines
//! on `\n` / `\r\n` / `\r` only. So if an earlier line contains one of the
//! extra Unicode break characters — e.g. inside a string literal — ropey's
//! line index runs *ahead* of the client's, and every subsequent position
//! maps to the wrong line (wrong byte offset for hover / goto-def /
//! references / rename, and an off-by-N line in the reverse direction).
//!
//! To stay client-compatible regardless of ropey's feature flags, the line
//! model here is `\n`-only: a line break is a single `\n`, and a preceding
//! `\r` (the `\r\n` case) is folded into that same break. Note this is
//! slightly stricter than the LSP "`\n` / `\r\n` / `\r`" set — a *lone* `\r`
//! is not treated as a break — but lone-CR line endings are vanishingly
//! rare in `.tf` sources, and crucially this never runs *ahead* of the
//! client the way the Unicode-break set does, so cursor positions on later
//! lines stay correct.

use lsp_types::{Position, Range};
use ropey::Rope;

use crate::error::ParseError;

/// `\n`-only line boundaries for the rope, as absolute byte offsets.
///
/// Returns the start byte of every line. The first entry is always `0`;
/// each subsequent entry is the byte immediately after a `\n`. The number
/// of lines is `line_starts.len()` (a trailing `\n` produces a final empty
/// line, matching how LSP clients count lines).
fn line_start_bytes(rope: &Rope) -> Vec<usize> {
    let mut starts = vec![0usize];
    // Iterate the underlying chunks to find `\n` bytes without paying a
    // full `to_string()` allocation. Chunk boundaries never split a
    // codepoint, and `\n` is a single byte, so a byte scan per chunk is
    // sufficient and correct.
    let mut byte_pos = 0usize;
    for chunk in rope.chunks() {
        for &b in chunk.as_bytes() {
            byte_pos += 1;
            if b == b'\n' {
                starts.push(byte_pos);
            }
        }
    }
    starts
}

/// Byte range `[start, end)` of the line at `line_idx` in the `\n`-only
/// model, given precomputed `line_starts`. `end` excludes nothing — it is
/// the start of the next line (or the document end for the last line), so
/// the slice includes any trailing `\n` / `\r\n`.
fn line_byte_range(line_starts: &[usize], line_idx: usize, total_bytes: usize) -> (usize, usize) {
    let start = line_starts[line_idx];
    let end = line_starts.get(line_idx + 1).copied().unwrap_or(total_bytes);
    (start, end)
}

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
    let line_starts = line_start_bytes(rope);
    let total_lines = line_starts.len();
    if line_idx >= total_lines {
        return Err(ParseError::LineOutOfBounds {
            line: pos.line,
            total_lines,
        });
    }

    let total_bytes = rope.len_bytes();
    let (line_start_byte, line_end_byte) = line_byte_range(&line_starts, line_idx, total_bytes);
    // `\n`-only line slice — may contain interior Unicode "line breaks"
    // (U+2028, U+0085, …) that the client treats as ordinary characters.
    let line = rope.byte_slice(line_start_byte..line_end_byte);

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

    let line_starts = line_start_bytes(rope);
    // Largest line index whose start byte is <= offset (the `\n`-only line
    // containing `offset`).
    let line_idx = match line_starts.binary_search(&offset) {
        Ok(idx) => idx,
        Err(idx) => idx - 1,
    };
    let line_start_byte = line_starts[line_idx];

    // Emit a UTF-16 code-unit column (LSP's default `Position.character`
    // encoding), not a byte difference — they diverge on multibyte lines.
    let character = rope
        .byte_slice(line_start_byte..offset)
        .len_utf16_cu();

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
        let offset =
            lsp_position_to_byte_offset(&r, Position::new(0, 0)).expect("start of empty doc");
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
        let offset =
            lsp_position_to_byte_offset(&r, Position::new(0, 50)).expect("CRLF visible length");
        assert_eq!(offset, r.line_to_byte(0) + 3);
    }

    #[test]
    fn byte_offset_rejects_out_of_bounds() {
        let r = rope("abc");
        let err = byte_offset_to_lsp_position(&r, 999);
        assert!(matches!(err, Err(ParseError::ByteOffsetOutOfBounds { .. })));
    }

    #[test]
    fn unicode_line_separator_does_not_split_lines() {
        // U+2028 (LINE SEPARATOR) inside a string literal on line 0.
        // ropey's default `unicode_lines` feature treats U+2028 as a line
        // break, so `rope.line()` would see an extra line and run ahead of
        // the LSP client, which splits on `\n` only. The symbol on the
        // (client-)second line must still map to its real byte offset.
        let src = "a = \"x\u{2028}y\"\nfoo = 1\n";
        let r = rope(src);

        // Byte offset of `foo` in the source (after the first `\n`).
        let foo_byte = src.find("foo").expect("foo present");
        // In the `\n`-only / client line model, `foo` is at line 1, col 0.
        let pos = Position::new(1, 0);

        let off = lsp_position_to_byte_offset(&r, pos).expect("valid position");
        assert_eq!(
            off, foo_byte,
            "U+2028 on line 0 must not shift line 1 — got byte {off}, want {foo_byte}"
        );

        // Round-trips back to the same line/character under the `\n` model.
        let back = byte_offset_to_lsp_position(&r, off).expect("valid offset");
        assert_eq!(back, pos, "round-trip line/char mismatch");
    }

    #[test]
    fn unicode_nel_does_not_split_lines() {
        // U+0085 (NEL) is also in ropey's `unicode_lines` set but not the
        // LSP break set. Same invariant as U+2028.
        let src = "a = \"x\u{0085}y\"\nbar = 2\n";
        let r = rope(src);
        let bar_byte = src.find("bar").expect("bar present");
        let pos = Position::new(1, 0);
        let off = lsp_position_to_byte_offset(&r, pos).expect("valid position");
        assert_eq!(off, bar_byte, "U+0085 on line 0 must not shift line 1");
        let back = byte_offset_to_lsp_position(&r, off).expect("valid offset");
        assert_eq!(back, pos);
    }

    #[test]
    fn span_converts_to_range() {
        let r = rope("hello\nworld");
        let range = hcl_span_to_lsp_range(&r, 0..5).expect("valid span");
        assert_eq!(range.start, Position::new(0, 0));
        assert_eq!(range.end, Position::new(0, 5));
    }
}
