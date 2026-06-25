//! Hover for Terraform built-in named values: `path.*`, `terraform.*`,
//! `count.*`, `each.*`, and `self`.
//!
//! These reference namespaces aren't declared anywhere in configuration, so
//! the symbol-table fallback can't describe them. This handler resolves the
//! `head` (and optional `.attr`) the cursor sits on and renders the
//! canonical docs from `tfls_core::named_value_description`.

use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range};
use tfls_parser::{byte_offset_to_lsp_position, lsp_position_to_byte_offset};
use tfls_state::DocumentState;

use crate::handlers::signature_help::identifier_at;

pub fn named_value_hover(doc: &DocumentState, pos: Position) -> Option<Hover> {
    let offset = lsp_position_to_byte_offset(&doc.rope, pos).ok()?;
    let text = doc.rope.to_string();

    let (word, span) = identifier_at(&text, offset)?;
    let bytes = text.as_bytes();

    // Resolve (head, attr, highlight-span) from the cursor word.
    let (head, attr, hl) = if tfls_core::is_named_value_head(&word) {
        // Cursor on the head — read a following `.attr` if present so
        // `path.modu|le`'s sibling case and `pa|th.module`'s head case both
        // land on the precise member. `self` carries no attr.
        let attr = read_attr_after(bytes, span.end);
        (word.clone(), attr, span.clone())
    } else if span.start > 0 && bytes[span.start - 1] == b'.' {
        // Cursor on an `.attr` segment — look back past the dot for the head.
        let (head, _hspan) = identifier_at(&text, span.start - 1)?;
        if !tfls_core::is_named_value_head(&head) {
            return None;
        }
        (head, Some(word.clone()), span.clone())
    } else {
        return None;
    };

    let desc = tfls_core::named_value_description(&head, attr.as_deref());
    if desc.is_empty() {
        return None;
    }

    let range = byte_offset_to_lsp_position(&doc.rope, hl.start)
        .ok()
        .zip(byte_offset_to_lsp_position(&doc.rope, hl.end).ok())
        .map(|(start, end)| Range { start, end });

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: desc.to_string(),
        }),
        range,
    })
}

/// If `text[from..]` is `.<ident>`, return the identifier. Used to pick up
/// the member when the cursor is on the namespace head (`path` → `module`).
fn read_attr_after(bytes: &[u8], from: usize) -> Option<String> {
    if from >= bytes.len() || bytes[from] != b'.' {
        return None;
    }
    let mut end = from + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if end == from + 1 {
        return None;
    }
    std::str::from_utf8(&bytes[from + 1..end])
        .ok()
        .map(str::to_string)
}
