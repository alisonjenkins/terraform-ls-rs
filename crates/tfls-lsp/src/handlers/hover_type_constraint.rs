//! Hover for type-constraint keywords inside a `type =` expression.
//!
//! Terraform's type-system vocabulary — `string`, `number`, `bool`, `any`,
//! `null`, `list`, `set`, `map`, `tuple`, `object`, `optional` — are not
//! functions and carry no provider schema, so the function/symbol hover
//! paths never describe them. This handler recognises the keyword under the
//! cursor and, when the cursor genuinely sits in a type-constraint position,
//! renders its canonical docs (most usefully `optional`'s
//! null-when-omitted behaviour).
//!
//! The type-expression guard (`tfls_core::in_type_expression`) prevents a
//! variable / local / output literally named `string` (etc.) from picking
//! up a type-constraint card outside `type =`.

use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range};
use tfls_parser::{byte_offset_to_lsp_position, lsp_position_to_byte_offset};
use tfls_state::DocumentState;

use crate::handlers::signature_help::identifier_at;

pub fn type_constraint_hover(doc: &DocumentState, pos: Position) -> Option<Hover> {
    let offset = lsp_position_to_byte_offset(&doc.rope, pos).ok()?;
    let text = doc.rope.to_string();

    let (word, span) = identifier_at(&text, offset)?;
    if !tfls_core::is_type_constraint_keyword(&word) {
        return None;
    }
    // Only describe the keyword when the cursor is actually in a type
    // expression — otherwise an identifier that merely shares the name
    // (`local.string`, a variable called `object`, …) would hijack hover.
    if !tfls_core::in_type_expression(text.get(..span.start)?) {
        return None;
    }

    let desc = tfls_core::type_constraint_description(&word);
    if desc.is_empty() {
        return None;
    }

    let value = format!("**type constraint** `{word}`\n\n{desc}");
    let range = byte_offset_to_lsp_position(&doc.rope, span.start)
        .ok()
        .zip(byte_offset_to_lsp_position(&doc.rope, span.end).ok())
        .map(|(start, end)| Range { start, end });

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range,
    })
}
