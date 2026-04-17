//! Cursor-position lookup helpers.
//!
//! Used by navigation handlers to find the reference (if any) at a given
//! LSP position, so goto-definition/find-references can resolve it
//! against the global indexes.

use lsp_types::Position;
use tfls_parser::Reference;

use crate::document::DocumentState;

/// Find the reference whose range contains `pos`. If multiple
/// references overlap (e.g. a traversal containing a shorter prefix
/// reference), the smallest matching range is returned — it most
/// closely matches what the user is pointing at.
pub fn reference_at_position(doc: &DocumentState, pos: Position) -> Option<&Reference> {
    doc.references
        .iter()
        .filter(|r| contains(&r.location.range(), pos))
        .min_by_key(|r| range_length(&r.location.range()))
}

fn contains(range: &lsp_types::Range, pos: Position) -> bool {
    let after_start = (pos.line, pos.character) >= (range.start.line, range.start.character);
    let before_end = (pos.line, pos.character) <= (range.end.line, range.end.character);
    after_start && before_end
}

fn range_length(range: &lsp_types::Range) -> u64 {
    let start = (range.start.line as u64) << 32 | (range.start.character as u64);
    let end = (range.end.line as u64) << 32 | (range.end.character as u64);
    end.saturating_sub(start)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::Url;
    use tfls_parser::ReferenceKind;

    fn uri() -> Url {
        Url::parse("file:///test.tf").expect("valid url")
    }

    #[test]
    fn finds_reference_at_cursor() {
        let src = r#"output "x" { value = var.region }"#;
        let doc = DocumentState::new(uri(), src, 1);
        // Cursor on `region`.
        let region_offset = src.find("region").unwrap() as u32;
        let pos = Position::new(0, region_offset + 2);
        let r = reference_at_position(&doc, pos).expect("should find reference");
        match &r.kind {
            ReferenceKind::Variable { name } => assert_eq!(name, "region"),
            other => panic!("wrong reference: {other:?}"),
        }
    }

    #[test]
    fn returns_none_outside_any_reference() {
        let doc = DocumentState::new(uri(), r#"output "x" { value = 42 }"#, 1);
        let pos = Position::new(0, 0);
        assert!(reference_at_position(&doc, pos).is_none());
    }
}
