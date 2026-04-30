//! `textDocument/prepareRename` + `textDocument/rename`.
//!
//! Rename works by walking the global `definitions_by_name` /
//! `references_by_name` indexes for the symbol under the cursor and
//! emitting a `WorkspaceEdit` with one `TextEdit` per location. Each
//! edit targets the *narrow* identifier range (the last dotted
//! segment for references, the block label for definitions) — not the
//! full span stored in `SymbolLocation`.
//!
//! The cursor can be on either a reference (`var.region`) or a
//! defining block label (`variable "region" {}`); both cases are
//! supported via [`find_symbol_at_cursor`].

use std::collections::HashMap;

use lsp_types::{
    PrepareRenameResponse, Range, RenameParams, TextDocumentPositionParams, TextEdit, Url,
    WorkspaceEdit,
};
use ropey::Rope;
use tfls_core::{SymbolKind, SymbolLocation};
use tfls_state::{DocumentState, StateStore, SymbolKey};
use tower_lsp::jsonrpc;

use crate::backend::Backend;
use crate::handlers::cursor::{CursorKind, find_symbol_at_cursor};

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
    // Provider-defined function alias rename:
    //   `provider::aws_v6::|fn(...)` → cursor on `aws_v6` segment
    //   produces a workspace-wide rename of the alias.
    if let Some(resp) = prepare_rename_provider_local(&doc, params.position) {
        return Ok(Some(resp));
    }
    let Some(target) = find_symbol_at_cursor(&doc, params.position) else {
        return Ok(None);
    };

    let name = target.key.name.clone();
    let full_range = target.location.range();
    let narrow = match target.kind {
        CursorKind::Reference => narrow_identifier_range(&doc.rope, full_range, &name),
        CursorKind::Definition => narrow_quoted_label(&doc.rope, full_range, &name),
    };
    let Some(narrow) = narrow else {
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

    // Provider local alias rename — workspace-wide. Tried BEFORE
    // the symbol-rename path so the `aws_v6` segment isn't
    // accidentally treated as a regular identifier.
    if let Some(doc) = backend.state.documents.get(&uri) {
        if let Some(edit) = rename_provider_local(
            &backend.state,
            &uri,
            &doc,
            params.text_document_position.position,
            &new_name,
        ) {
            return Ok(Some(edit));
        }
    }

    let key = {
        let Some(doc) = backend.state.documents.get(&uri) else {
            return Ok(None);
        };
        let Some(target) = find_symbol_at_cursor(&doc, params.text_document_position.position)
        else {
            return Ok(None);
        };
        target.key
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

// Silence clippy for unused-but-exported helpers.
#[allow(dead_code)]
fn _docs(_s: &StateStore, _k: SymbolKey, _d: &DocumentState) {}
#[allow(dead_code)]
fn _symkind_noop(_k: SymbolKind) {}

/// `prepareRename` for the LOCAL segment of `provider::LOCAL::fn(...)`.
/// Returns the narrow range of the alias under the cursor with the
/// alias text as the placeholder.
fn prepare_rename_provider_local(
    doc: &DocumentState,
    pos: lsp_types::Position,
) -> Option<PrepareRenameResponse> {
    let offset = tfls_parser::lsp_position_to_byte_offset(&doc.rope, pos).ok()?;
    let text = doc.rope.to_string();
    let (start, end, local) = local_span_at(&text, offset)?;
    let start_pos = tfls_parser::byte_offset_to_lsp_position(&doc.rope, start).ok()?;
    let end_pos = tfls_parser::byte_offset_to_lsp_position(&doc.rope, end).ok()?;
    Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: Range {
            start: start_pos,
            end: end_pos,
        },
        placeholder: local,
    })
}

/// Cursor on a LOCAL segment? Return its byte span (start, end) and
/// the alias string. Walks the identifier surrounding `offset` and
/// confirms it's preceded by `provider::`.
fn local_span_at(text: &str, offset: usize) -> Option<(usize, usize, String)> {
    let bytes = text.as_bytes();
    let mut start = offset;
    while start > 0
        && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
    {
        start -= 1;
    }
    let mut end = offset;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
    {
        end += 1;
    }
    if start == end {
        return None;
    }
    if start < 10 || &bytes[start - 10..start] != b"provider::" {
        return None;
    }
    if start > 10 {
        let prev = bytes[start - 11];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return None;
        }
    }
    Some((start, end, text[start..end].to_string()))
}

/// Build a `WorkspaceEdit` that renames a provider local alias
/// across:
///   - the `LOCAL = { ... }` attribute key in
///     `terraform { required_providers { ... } }` (every peer file
///     in the same module dir).
///   - every `provider::LOCAL::*` call site in every doc in the
///     workspace.
fn rename_provider_local(
    state: &StateStore,
    uri: &Url,
    doc: &DocumentState,
    pos: lsp_types::Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let offset = tfls_parser::lsp_position_to_byte_offset(&doc.rope, pos).ok()?;
    let text = doc.rope.to_string();
    let (_, _, old_local) = local_span_at(&text, offset)?;

    let mut edits: HashMap<Url, Vec<TextEdit>> = HashMap::new();

    // 1. required_providers attribute key — module-dir peer walk.
    push_required_providers_attr_edits(state, uri, &old_local, new_name, &mut edits);

    // 2. Every `provider::OLD::fn` call site, workspace-wide.
    for entry in state.documents.iter() {
        push_call_site_edits(
            entry.key(),
            entry.value(),
            &old_local,
            new_name,
            &mut edits,
        );
    }

    if edits.is_empty() {
        return None;
    }
    Some(WorkspaceEdit {
        changes: Some(edits),
        ..Default::default()
    })
}

fn push_required_providers_attr_edits(
    state: &StateStore,
    uri: &Url,
    old_local: &str,
    new_name: &str,
    edits: &mut HashMap<Url, Vec<TextEdit>>,
) {
    let mut try_doc = |doc_uri: &Url, doc: &DocumentState| {
        let Some(body) = doc.parsed.body.as_ref() else {
            return;
        };
        if let Some(range) = required_providers_key_range(body, &doc.rope, old_local) {
            edits
                .entry(doc_uri.clone())
                .or_default()
                .push(TextEdit {
                    range,
                    new_text: new_name.to_string(),
                });
        }
    };
    if let Some(doc) = state.documents.get(uri) {
        try_doc(uri, doc.value());
    }
    let Some(target_dir) = crate::handlers::util::parent_dir(uri) else {
        return;
    };
    for entry in state.documents.iter() {
        let other_uri = entry.key();
        if other_uri == uri {
            continue;
        }
        let Ok(path) = other_uri.to_file_path() else {
            continue;
        };
        if path.parent() != Some(target_dir.as_path()) {
            continue;
        }
        try_doc(other_uri, entry.value());
    }
}

fn required_providers_key_range(
    body: &hcl_edit::structure::Body,
    rope: &Rope,
    local: &str,
) -> Option<Range> {
    use hcl_edit::repr::Span as _;
    for structure in body.iter() {
        let Some(block) = structure.as_block() else { continue };
        if block.ident.as_str() != "terraform" {
            continue;
        }
        for inner in block.body.iter() {
            let Some(rp_block) = inner.as_block() else { continue };
            if rp_block.ident.as_str() != "required_providers" {
                continue;
            }
            for entry in rp_block.body.iter() {
                let Some(attr) = entry.as_attribute() else { continue };
                if attr.key.as_str() != local {
                    continue;
                }
                let span = attr.key.span()?;
                let start = tfls_parser::byte_offset_to_lsp_position(rope, span.start).ok()?;
                let end = tfls_parser::byte_offset_to_lsp_position(rope, span.end).ok()?;
                return Some(Range { start, end });
            }
        }
    }
    None
}

fn push_call_site_edits(
    doc_uri: &Url,
    doc: &DocumentState,
    old_local: &str,
    new_name: &str,
    edits: &mut HashMap<Url, Vec<TextEdit>>,
) {
    let text = doc.rope.to_string();
    let bytes = text.as_bytes();
    let needle = format!("provider::{old_local}::");
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find(&needle) {
        let abs = search_from + rel;
        let abs_end = abs + needle.len();
        // Boundary check: `provider` must not be tail of longer
        // ident.
        let prev_ok = abs == 0 || {
            let p = bytes[abs - 1];
            !(p.is_ascii_alphanumeric() || p == b'_' || p == b':')
        };
        if !prev_ok {
            search_from = abs_end;
            continue;
        }
        // Must be followed by an identifier (the fn name) — protects
        // against false positives if `LOCAL::` ends a different
        // construct.
        let after = abs_end;
        if after >= bytes.len() {
            search_from = abs_end;
            continue;
        }
        let next = bytes[after];
        if !(next.is_ascii_alphabetic() || next == b'_') {
            search_from = abs_end;
            continue;
        }
        // Range covers JUST the OLD local segment, not the whole
        // `provider::OLD::` prefix — narrow edits compose better.
        let local_start = abs + "provider::".len();
        let local_end = local_start + old_local.len();
        let Ok(start_pos) = tfls_parser::byte_offset_to_lsp_position(&doc.rope, local_start)
        else {
            search_from = abs_end;
            continue;
        };
        let Ok(end_pos) = tfls_parser::byte_offset_to_lsp_position(&doc.rope, local_end) else {
            search_from = abs_end;
            continue;
        };
        edits
            .entry(doc_uri.clone())
            .or_default()
            .push(TextEdit {
                range: Range {
                    start: start_pos,
                    end: end_pos,
                },
                new_text: new_name.to_string(),
            });
        search_from = abs_end;
    }
}

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
