//! Per-document state: rope buffer, parsed AST, version, diagnostics,
//! symbol table, references.

use lsp_types::{TextDocumentContentChangeEvent, Url};
use ropey::Rope;
use tfls_core::SymbolTable;
use tfls_parser::{
    ParsedFile, Reference, extract_references, extract_symbols, lsp_position_to_byte_offset,
    parse_source,
};

use crate::error::StateError;

/// Mutable state for a single open document.
///
/// `symbols` and `references` reflect the last successful parse. This
/// preserves useful navigation/completion data even while the user
/// has the document in a transiently broken state.
#[derive(Debug)]
pub struct DocumentState {
    pub uri: Url,
    pub rope: Rope,
    pub version: i32,
    pub parsed: ParsedFile,
    pub symbols: SymbolTable,
    pub references: Vec<Reference>,
}

impl DocumentState {
    pub fn new(uri: Url, text: &str, version: i32) -> Self {
        let rope = Rope::from_str(text);
        let parsed = parse_source(text);
        let (symbols, references) = compute_analysis(&parsed, &uri, &rope);
        Self {
            uri,
            rope,
            version,
            parsed,
            symbols,
            references,
        }
    }

    pub fn apply_change(
        &mut self,
        change: TextDocumentContentChangeEvent,
    ) -> Result<(), StateError> {
        match change.range {
            Some(range) => {
                let start = lsp_position_to_byte_offset(&self.rope, range.start).map_err(
                    |source| StateError::EditApplication {
                        uri: self.uri.to_string(),
                        source,
                    },
                )?;
                let end = lsp_position_to_byte_offset(&self.rope, range.end).map_err(|source| {
                    StateError::EditApplication {
                        uri: self.uri.to_string(),
                        source,
                    }
                })?;

                let start_char = self.rope.byte_to_char(start);
                let end_char = self.rope.byte_to_char(end);
                self.rope.remove(start_char..end_char);
                self.rope.insert(start_char, &change.text);
            }
            None => {
                self.rope = Rope::from_str(&change.text);
            }
        }
        Ok(())
    }

    /// Re-parse and re-analyse the document's current rope content.
    /// Symbols/references from the last successful parse are retained
    /// when the new parse fails (no body produced), so navigation
    /// keeps working while the user is mid-edit.
    pub fn reparse(&mut self) {
        let text = self.rope.to_string();
        self.parsed = parse_source(&text);
        if self.parsed.body.is_some() {
            let (symbols, references) =
                compute_analysis(&self.parsed, &self.uri, &self.rope);
            self.symbols = symbols;
            self.references = references;
        }
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }
}

fn compute_analysis(
    parsed: &ParsedFile,
    uri: &Url,
    rope: &Rope,
) -> (SymbolTable, Vec<Reference>) {
    match &parsed.body {
        Some(body) => {
            let symbols = extract_symbols(body, uri, rope);
            let references = extract_references(body, uri, rope);
            (symbols, references)
        }
        None => (SymbolTable::new(), Vec::new()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn test_uri() -> Url {
        Url::parse("file:///test.tf").expect("valid url")
    }

    #[test]
    fn new_document_parses_initial_text() {
        let doc = DocumentState::new(test_uri(), "variable \"x\" {}", 1);
        assert_eq!(doc.version, 1);
        assert!(doc.parsed.body.is_some());
        assert_eq!(doc.symbols.variables.len(), 1);
    }

    #[test]
    fn apply_full_replacement() {
        let mut doc = DocumentState::new(test_uri(), "original", 1);
        doc.apply_change(TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "replaced".to_string(),
        })
        .expect("full replacement should apply");
        assert_eq!(doc.text(), "replaced");
    }

    #[test]
    fn apply_incremental_change() {
        let mut doc = DocumentState::new(test_uri(), "hello world", 1);
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position::new(0, 6),
                end: Position::new(0, 11),
            }),
            range_length: None,
            text: "rust".to_string(),
        })
        .expect("incremental change should apply");
        assert_eq!(doc.text(), "hello rust");
    }

    #[test]
    fn reparse_updates_symbols() {
        let mut doc = DocumentState::new(test_uri(), "", 1);
        assert!(doc.symbols.is_empty());
        doc.apply_change(TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "variable \"region\" {}".to_string(),
        })
        .expect("change should apply");
        doc.reparse();
        assert_eq!(doc.symbols.variables.len(), 1);
        assert!(doc.symbols.variables.contains_key("region"));
    }

    #[test]
    fn reparse_updates_references() {
        let doc = DocumentState::new(
            test_uri(),
            r#"output "x" { value = var.region }"#,
            1,
        );
        assert!(!doc.references.is_empty());
    }

    #[test]
    fn incremental_change_with_invalid_position_errors() {
        let mut doc = DocumentState::new(test_uri(), "short", 1);
        let err = doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position::new(99, 0),
                end: Position::new(99, 5),
            }),
            range_length: None,
            text: "x".to_string(),
        });
        assert!(matches!(err, Err(StateError::EditApplication { .. })));
    }
}
