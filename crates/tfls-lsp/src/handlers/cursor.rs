//! Shared cursor-position logic used by hover, rename, and references.
//!
//! When the cursor is on an identifier, callers need to know three things:
//! - the [`SymbolKey`] (which scopes index lookups), so `definitions_by_name`
//!   and `references_by_name` can be walked;
//! - whether the cursor is on a **reference** (`var.x`) or on the **defining
//!   block label** (`variable "x" {}`), so narrowing logic can pick between
//!   tail-identifier and quoted-label strategies;
//! - the full [`SymbolLocation`] of whatever is under the cursor, so the
//!   caller can narrow it down to a tight identifier range.
//!
//! The old code only handled the reference case (via `reference_at_position`),
//! which meant hover and rename silently returned `None` for any cursor
//! position on a definition label. This module adds the missing symbol-table
//! fallback.

use lsp_types::Position;
use tfls_core::{SymbolKind, SymbolLocation};
use tfls_state::{DocumentState, SymbolKey, reference_at_position, reference_key};

/// Whether the target under the cursor is a symbol definition (block label)
/// or a reference (`var.x`, `local.y`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    /// Cursor is on a reference (e.g. `var.region`).
    Reference,
    /// Cursor is on the defining block of a symbol.
    Definition,
}

/// Symbol information gathered from the cursor position. See the module-level
/// docs for why each field is needed.
#[derive(Debug, Clone)]
pub struct CursorTarget {
    pub key: SymbolKey,
    pub location: SymbolLocation,
    pub kind: CursorKind,
}

/// Resolve the symbol under the cursor in `doc`, whether the cursor is on a
/// reference or on the defining block label.
pub fn find_symbol_at_cursor(doc: &DocumentState, pos: Position) -> Option<CursorTarget> {
    if let Some(r) = reference_at_position(doc, pos) {
        return Some(CursorTarget {
            key: reference_key(&r.kind),
            location: r.location.clone(),
            kind: CursorKind::Reference,
        });
    }

    for (name, sym) in &doc.symbols.variables {
        if contains(&sym.location, pos) {
            return Some(CursorTarget {
                key: SymbolKey::new(SymbolKind::Variable, name),
                location: sym.location.clone(),
                kind: CursorKind::Definition,
            });
        }
    }
    for (name, sym) in &doc.symbols.locals {
        if contains(&sym.location, pos) {
            return Some(CursorTarget {
                key: SymbolKey::new(SymbolKind::Local, name),
                location: sym.location.clone(),
                kind: CursorKind::Definition,
            });
        }
    }
    for (name, sym) in &doc.symbols.outputs {
        if contains(&sym.location, pos) {
            return Some(CursorTarget {
                key: SymbolKey::new(SymbolKind::Output, name),
                location: sym.location.clone(),
                kind: CursorKind::Definition,
            });
        }
    }
    for (name, sym) in &doc.symbols.modules {
        if contains(&sym.location, pos) {
            return Some(CursorTarget {
                key: SymbolKey::new(SymbolKind::Module, name),
                location: sym.location.clone(),
                kind: CursorKind::Definition,
            });
        }
    }
    for (addr, sym) in &doc.symbols.resources {
        if contains(&sym.location, pos) {
            return Some(CursorTarget {
                key: SymbolKey::resource(SymbolKind::Resource, &addr.resource_type, &addr.name),
                location: sym.location.clone(),
                kind: CursorKind::Definition,
            });
        }
    }
    for (addr, sym) in &doc.symbols.data_sources {
        if contains(&sym.location, pos) {
            return Some(CursorTarget {
                key: SymbolKey::resource(SymbolKind::DataSource, &addr.resource_type, &addr.name),
                location: sym.location.clone(),
                kind: CursorKind::Definition,
            });
        }
    }

    None
}

/// Convenience wrapper used by callers that only need the key (e.g. the
/// `references` handler, which doesn't care about narrow ranges).
pub fn key_at_cursor(doc: &DocumentState, pos: Position) -> Option<SymbolKey> {
    find_symbol_at_cursor(doc, pos).map(|t| t.key)
}

fn contains(loc: &SymbolLocation, pos: Position) -> bool {
    let range = loc.range();
    let after_start = (pos.line, pos.character) >= (range.start.line, range.start.character);
    let before_end = (pos.line, pos.character) <= (range.end.line, range.end.character);
    after_start && before_end
}
