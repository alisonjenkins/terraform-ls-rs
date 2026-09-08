//! Formatter-diff scanning shared by the diagnostics pipeline
//! (`terraform_fmt` diagnostic) and `tfls-lsp`'s format code action.
//!
//! Transport-free: reads only `DocumentState` / `ropey::Rope`, no
//! `Backend` / tower-lsp dependency.

use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, TextEdit};
use ropey::Rope;
use tfls_state::DocumentState;

/// The end must be a UTF-16 code-unit column to match the server's default
/// positionEncoding (every other position the server emits goes through
/// `byte_offset_to_lsp_position`, which uses `len_utf16_cu()`). Using
/// `len_chars()` undercounts on a non-newline-terminated final line that
/// contains a non-BMP scalar (an emoji is 1 char but 2 UTF-16 units), so a
/// whole-document replace would leave the trailing UTF-16 unit(s) appended
/// after the formatted text — silent buffer corruption. Anchoring the end
/// at the rope's last byte offset reuses the exact same conversion.
///
/// Local copy of `tfls_lsp::handlers::formatting::whole_document_range`
/// (a private helper, not shared across the crate boundary).
fn whole_document_range(rope: &Rope) -> Range {
    let end = tfls_parser::byte_offset_to_lsp_position(rope, rope.len_bytes())
        .unwrap_or_else(|_| Position::new(rope.len_lines().saturating_sub(1) as u32, 0));
    Range {
        start: Position::new(0, 0),
        end,
    }
}

/// Format a single document under the active style. Returns a
/// whole-file `TextEdit` when the formatted output differs from
/// the input; `None` when the doc is already formatted, the
/// rope is empty, or the formatter rejected the source (parse
/// error, etc).
///
/// Pure — no LSP-state access. Caller decides which docs to
/// scan and which style to use.
fn scan_format(rope: &Rope, style: tfls_state::FormatStyle) -> Option<TextEdit> {
    let text = rope.to_string();
    let formatted = tfls_format::format_source(&text, style).ok()?;
    if formatted == text {
        return None;
    }
    Some(TextEdit {
        range: whole_document_range(rope),
        new_text: formatted,
    })
}

/// Cross-call wrapper around `scan_format`. Reads / writes the
/// document's own `format_cache` slot, invalidated on every
/// `apply_change` / `reparse`. Cache key is
/// `(DocumentState::version, FormatStyle::marker)` — a doc
/// edit bumps the version, a runtime style toggle bumps the
/// marker, both make stale entries miss.
///
/// Falls back to a fresh `scan_format` call when the cache
/// mutex is poisoned (lock failure is rare and recovery is
/// cheap — just don't bypass the formatter).
pub fn scan_format_cached(doc: &DocumentState, style: tfls_state::FormatStyle) -> Option<TextEdit> {
    let style_marker = style.marker();
    if let Ok(guard) = doc.format_cache.lock() {
        if let Some(entry) = guard.as_ref() {
            if entry.version == doc.version && entry.style_marker == style_marker {
                return entry.edit.clone();
            }
        }
    }
    let edit = scan_format(&doc.rope, style);
    if let Ok(mut guard) = doc.format_cache.lock() {
        *guard = Some(tfls_state::FormatCacheEntry {
            version: doc.version,
            style_marker,
            edit: edit.clone(),
        });
    }
    edit
}

/// `terraform_fmt` — INFORMATION diagnostic when the document isn't
/// formatted to the active style (minimal = `terraform fmt`/`tofu fmt`
/// parity, opinionated = full tf-format). Reuses the cached format scan
/// so an already-formatted, unchanged buffer is a no-op. Ranges at the
/// first line that differs from the formatted output; pairs with the
/// existing format code action. Default-on; disable / retune via the
/// per-rule config (`{"rules": {"terraform_fmt": "off"}}`).
pub fn formatting_diagnostic(
    doc: &DocumentState,
    style: tfls_state::FormatStyle,
) -> Option<Diagnostic> {
    let edit = scan_format_cached(doc, style)?; // `None` ⇒ already formatted.
    let formatted = &edit.new_text;
    let original = doc.rope.to_string();

    // First line whose content differs — a friendlier anchor than line 0.
    let line = original
        .lines()
        .zip(formatted.lines())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| {
            original
                .lines()
                .count()
                .min(formatted.lines().count())
                .saturating_sub(1)
        });
    let line_len = original
        .lines()
        .nth(line)
        .map(|l| l.chars().count())
        .unwrap_or(0);

    let style_name = match style {
        tfls_state::FormatStyle::Minimal => "terraform fmt",
        tfls_state::FormatStyle::Opinionated => "opinionated tf-format",
    };
    Some(Diagnostic {
        range: Range {
            start: Position::new(line as u32, 0),
            end: Position::new(line as u32, line_len as u32),
        },
        severity: Some(DiagnosticSeverity::INFORMATION),
        source: Some("terraform-ls-rs".to_string()),
        message: format!("File is not formatted ({style_name} style); run the formatter."),
        ..Default::default()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

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
}
