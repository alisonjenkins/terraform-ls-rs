//! `textDocument/prepareRename` + `textDocument/rename`.
//!
//! Rename works by walking the global `definitions_by_name` /
//! `references_by_name` indexes for the symbol under the cursor and
//! emitting a `WorkspaceEdit` with one `TextEdit` per location. Each
//! edit targets the *narrow* identifier range (the last dotted
//! segment for references, the block label for definitions) — not the
//! full span stored in `SymbolLocation`.

use std::collections::HashMap;

use lsp_types::{
    PrepareRenameResponse, Range, RenameParams, TextDocumentPositionParams, TextEdit, Url,
    WorkspaceEdit,
};
use ropey::Rope;
use tfls_core::{SymbolKind, SymbolLocation};
use tfls_state::{DocumentState, StateStore, SymbolKey, reference_at_position, reference_key};
use tfls_parser::ReferenceKind;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

/// Validate that a rename may happen here; return the range of the
/// identifier being renamed so the editor can highlight it.
pub async fn prepare_rename(
    backend: &Backend,
    params: TextDocumentPositionParams,
) -> jsonrpc::Result<Option<PrepareRenameResponse>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let Some(reference) = reference_at_position(&doc, params.position) else {
        return Ok(None);
    };

    let name = reference_name(&reference.kind);
    let full_range = reference.location.range();
    let Some(narrow) = narrow_identifier_range(&doc.rope, full_range, &name) else {
        return Ok(None);
    };

    Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: narrow,
        placeholder: name,
    }))
}

/// Produce a `WorkspaceEdit` that renames every occurrence of the
/// symbol under the cursor to `new_name`, across all indexed files.
pub async fn rename(
    backend: &Backend,
    params: RenameParams,
) -> jsonrpc::Result<Option<WorkspaceEdit>> {
    let uri = params.text_document_position.text_document.uri;
    let new_name = params.new_name;

    let key = {
        let Some(doc) = backend.state.documents.get(&uri) else {
            return Ok(None);
        };
        let Some(reference) = reference_at_position(&doc, params.text_document_position.position) else {
            return Ok(None);
        };
        reference_key(&reference.kind)
    };

    let mut edits: HashMap<Url, Vec<TextEdit>> = HashMap::new();

    // Definitions — label rename.
    if let Some(defs) = backend.state.definitions_by_name.get(&key) {
        for loc in defs.iter() {
            push_narrow_edit(backend, loc, &key.name, &new_name, &mut edits, EditKind::Label);
        }
    }

    // References — tail-identifier rename.
    if let Some(refs) = backend.state.references_by_name.get(&key) {
        for loc in refs.iter() {
            push_narrow_edit(backend, loc, &key.name, &new_name, &mut edits, EditKind::Tail);
        }
    }

    if edits.is_empty() {
        Ok(None)
    } else {
        Ok(Some(WorkspaceEdit {
            changes: Some(edits),
            ..Default::default()
        }))
    }
}

enum EditKind {
    /// Reference like `var.region` — replace the last dotted segment.
    Tail,
    /// Definition like `variable "region" { ... }` — replace the label.
    Label,
}

fn push_narrow_edit(
    backend: &Backend,
    loc: &SymbolLocation,
    old_name: &str,
    new_name: &str,
    edits: &mut HashMap<Url, Vec<TextEdit>>,
    kind: EditKind,
) {
    // Resolve the narrow range inside the location's full range.
    let narrow = match kind {
        EditKind::Tail => narrow_tail_identifier(backend, loc, old_name),
        EditKind::Label => narrow_label_identifier(backend, loc, old_name),
    };

    let Some(range) = narrow else {
        tracing::debug!(uri = %loc.uri, "rename: could not narrow range, skipping");
        return;
    };

    edits
        .entry(loc.uri.clone())
        .or_default()
        .push(TextEdit {
            range,
            new_text: new_name.to_string(),
        });
}

fn narrow_tail_identifier(
    backend: &Backend,
    loc: &SymbolLocation,
    old_name: &str,
) -> Option<Range> {
    let doc = backend.state.documents.get(&loc.uri)?;
    narrow_identifier_range(&doc.rope, loc.range(), old_name)
}

fn narrow_label_identifier(
    backend: &Backend,
    loc: &SymbolLocation,
    old_name: &str,
) -> Option<Range> {
    let doc = backend.state.documents.get(&loc.uri)?;
    narrow_quoted_label(&doc.rope, loc.range(), old_name)
}

/// Scan the rope text covered by `range` to find the last occurrence
/// of `name` as a whole identifier word, returning its narrow LSP range.
pub(crate) fn narrow_identifier_range(
    rope: &Rope,
    range: Range,
    name: &str,
) -> Option<Range> {
    let text = slice_rope(rope, range)?;
    let rel_start = rfind_ident(&text, name)?;
    // Convert back to absolute positions.
    absolute_range(rope, range, rel_start, rel_start + name.len())
}

/// Like `narrow_identifier_range` but looks for `"<name>"` (the first
/// quoted label matching `name`). Used for definitions where the name
/// appears as a block label.
pub(crate) fn narrow_quoted_label(
    rope: &Rope,
    range: Range,
    name: &str,
) -> Option<Range> {
    let text = slice_rope(rope, range)?;
    let needle = format!("\"{name}\"");
    let rel_pos = text.find(&needle)?;
    let rel_start = rel_pos + 1; // skip opening quote
    absolute_range(rope, range, rel_start, rel_start + name.len())
}

fn slice_rope(rope: &Rope, range: Range) -> Option<String> {
    let start = tfls_parser::lsp_position_to_byte_offset(rope, range.start).ok()?;
    let end = tfls_parser::lsp_position_to_byte_offset(rope, range.end).ok()?;
    if end < start || end > rope.len_bytes() {
        return None;
    }
    let start_char = rope.byte_to_char(start);
    let end_char = rope.byte_to_char(end);
    Some(rope.slice(start_char..end_char).to_string())
}

fn absolute_range(
    rope: &Rope,
    outer: Range,
    rel_start: usize,
    rel_end: usize,
) -> Option<Range> {
    let base = tfls_parser::lsp_position_to_byte_offset(rope, outer.start).ok()?;
    let start = tfls_parser::byte_offset_to_lsp_position(rope, base + rel_start).ok()?;
    let end = tfls_parser::byte_offset_to_lsp_position(rope, base + rel_end).ok()?;
    Some(Range { start, end })
}

/// Rightmost occurrence of `name` in `haystack` where surrounding
/// bytes are not identifier chars — i.e. a whole-word match.
fn rfind_ident(haystack: &str, name: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let nlen = name.len();
    if nlen == 0 || bytes.len() < nlen {
        return None;
    }
    let mut pos = bytes.len() - nlen;
    loop {
        if &bytes[pos..pos + nlen] == name.as_bytes() {
            let before_ok = pos == 0 || !is_ident_byte(bytes[pos - 1]);
            let after_ok = pos + nlen == bytes.len() || !is_ident_byte(bytes[pos + nlen]);
            if before_ok && after_ok {
                return Some(pos);
            }
        }
        if pos == 0 {
            return None;
        }
        pos -= 1;
    }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn reference_name(kind: &ReferenceKind) -> String {
    match kind {
        ReferenceKind::Variable { name }
        | ReferenceKind::Local { name }
        | ReferenceKind::Module { name } => name.clone(),
        ReferenceKind::Resource { name, .. } | ReferenceKind::DataSource { name, .. } => name.clone(),
    }
}

// Silence clippy for unused-but-exported helpers.
#[allow(dead_code)]
fn _docs(_s: &StateStore, _k: SymbolKey, _d: &DocumentState) {}
#[allow(dead_code)]
fn _symkind_noop(_k: SymbolKind) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::Position;

    fn r(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range {
            start: Position::new(sl, sc),
            end: Position::new(el, ec),
        }
    }

    #[test]
    fn rfind_ident_whole_word_only() {
        assert_eq!(rfind_ident("abc var.region def", "region"), Some(8));
        // Not a match: surrounded by ident chars.
        assert_eq!(rfind_ident("regionally", "region"), None);
        assert_eq!(rfind_ident("my_region_", "region"), None);
    }

    #[test]
    fn rfind_ident_picks_rightmost() {
        assert_eq!(rfind_ident("region=var.region", "region"), Some(11));
    }

    #[test]
    fn narrow_identifier_picks_tail_segment() {
        let rope = Rope::from_str("output \"x\" { value = var.region }\n");
        // Range covers `var.region` (cols 21..31).
        let narrow = narrow_identifier_range(&rope, r(0, 21, 0, 31), "region").expect("narrow");
        assert_eq!(narrow.start, Position::new(0, 25));
        assert_eq!(narrow.end, Position::new(0, 31));
    }

    #[test]
    fn narrow_quoted_label_picks_label_text() {
        let src = "variable \"region\" { default = \"x\" }\n";
        let rope = Rope::from_str(src);
        // Range covers whole block (cols 0..35).
        let narrow = narrow_quoted_label(&rope, r(0, 0, 0, 35), "region").expect("narrow");
        // 'region' starts at col 10 (after `variable "`).
        assert_eq!(narrow.start, Position::new(0, 10));
        assert_eq!(narrow.end, Position::new(0, 16));
    }

    #[test]
    fn narrow_quoted_label_returns_none_when_missing() {
        let rope = Rope::from_str("variable \"other\" {}\n");
        assert!(narrow_quoted_label(&rope, r(0, 0, 0, 19), "region").is_none());
    }
}
