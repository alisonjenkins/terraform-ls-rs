//! Per-document state: rope buffer, parsed AST, version, diagnostics,
//! symbol table, references.

use std::sync::Mutex;

use lsp_types::{TextDocumentContentChangeEvent, TextEdit};
use ropey::Rope;
use tfls_core::SymbolTable;
use tfls_parser::{
    extract_references, extract_references_fallback, extract_symbols, extract_symbols_fallback,
    lsp_position_to_byte_offset, parse_source_recovering_for_uri, ParsedFile, Reference,
};
use url::Url;

use crate::error::StateError;

/// Cached output of a previous `format_source` pass on the
/// document's rope. Keyed by document `version` (LSP-tracked,
/// monotonic per change) + format-style marker. Invalidated by
/// `apply_change` clearing the slot. Lets the LSP code-action
/// handler skip the O(file size) format pass when nothing has
/// changed since the last invocation.
#[derive(Debug, Clone)]
pub struct FormatCacheEntry {
    /// `DocumentState::version` snapshot the cached edit
    /// belongs to. Mismatch ⇒ rope changed ⇒ cache stale.
    pub version: i32,
    /// Format style the output was produced under. Different
    /// styles produce different formatted output, so the cache
    /// must invalidate when the user toggles style at runtime.
    pub style_marker: u8,
    /// `Some(edit)` — formatter changed the source; apply
    /// `edit` to format. `None` — source already formatted; no
    /// edit needed.
    pub edit: Option<TextEdit>,
}

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
    /// Last format-source pass result. `Mutex` because the LSP
    /// handlers see a shared `&DocumentState` (DashMap shard
    /// guard) but may need to populate the cache on first
    /// call. Cleared whenever the rope changes — see
    /// `apply_change` and `reparse`. `Option<FormatCacheEntry>`
    /// inside: `None` = never computed, `Some(entry)` = match
    /// against `entry.content_hash` + `entry.style_marker`.
    pub format_cache: Mutex<Option<FormatCacheEntry>>,
}

impl DocumentState {
    pub fn new(uri: Url, text: &str, version: i32) -> Self {
        let rope = Rope::from_str(text);
        let parsed = parse_source_recovering_for_uri(text, uri.as_str());
        let (symbols, references) = compute_analysis(&parsed, &uri, &rope);
        Self {
            uri,
            rope,
            version,
            parsed,
            symbols,
            references,
            format_cache: Mutex::new(None),
        }
    }

    /// Build a document from cached symbols + references (no
    /// parsed AST). Used by [`crate::index_cache::IndexCache`] on
    /// workspace re-open to restore cross-file index state without
    /// paying the parse cost again. `parsed.body` is `None`, so
    /// body-dependent diagnostic rules will skip this document
    /// until the user opens it (at which point `did_open` calls
    /// the full [`Self::new`] constructor and everything comes
    /// online).
    pub fn hydrated_from_cache(
        uri: Url,
        text: &str,
        symbols: SymbolTable,
        references: Vec<Reference>,
    ) -> Self {
        let rope = Rope::from_str(text);
        // Synthesise a `ParsedFile` with no body — we
        // deliberately skipped the parse. `compute_diagnostics`
        // guards on `parsed.body.is_some()` for every
        // body-dependent rule.
        let parsed = ParsedFile {
            body: None,
            errors: Vec::new(),
        };
        Self {
            uri,
            rope,
            version: 0,
            parsed,
            symbols,
            references,
            format_cache: Mutex::new(None),
        }
    }

    pub fn apply_change(
        &mut self,
        change: TextDocumentContentChangeEvent,
    ) -> Result<(), StateError> {
        match change.range {
            Some(range) => {
                let start =
                    lsp_position_to_byte_offset(&self.rope, range.start).map_err(|source| {
                        StateError::EditApplication {
                            uri: self.uri.to_string(),
                            source,
                        }
                    })?;
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
        // Rope changed — any cached format result is stale.
        if let Ok(mut guard) = self.format_cache.lock() {
            *guard = None;
        }
        Ok(())
    }

    /// Apply a versioned `did_change` batch, tolerating the out-of-order
    /// delivery that tower-lsp-server's `buffer_unordered` pump (and lspmux's
    /// multiplexing) can produce.
    ///
    /// Because the server advertises FULL document sync, each batch is the
    /// complete document text at `version`. A newer version therefore
    /// supersedes any older one wholesale: apply it and advance. An
    /// older-or-equal `version` is a stale/duplicate replay and is dropped so
    /// it can't overwrite newer text. Returns `Ok(Some(version))` when the
    /// rope advanced, or `Ok(None)` for a dropped stale batch.
    pub fn apply_versioned_changes(
        &mut self,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Result<Option<i32>, StateError> {
        // The server advertises FULL document sync (see
        // `tfls_lsp::capabilities`): every did_change carries the COMPLETE
        // document text at `version`, so a newer version supersedes any older
        // one outright. Drop a stale or duplicate (older-or-equal) version;
        // otherwise apply and advance.
        //
        // This keeps the rope correct under tower-lsp's concurrent
        // notification handling (and lspmux's multiplexing): out-of-order
        // delivery just means a newer full-text edit may land first and the
        // older one is harmlessly dropped — there are no incremental ranges
        // to misapply, and no version-gap buffering that could freeze the
        // rope (the root of a class of "stuck / stale" bugs).
        if version <= self.version {
            return Ok(None);
        }
        for change in changes {
            self.apply_change(change)?;
        }
        self.version = version;
        Ok(Some(version))
    }

    /// Re-parse and re-analyse the document's current rope content.
    /// Symbols/references from the last successful parse are retained
    /// when the new parse fails (no body produced), so navigation
    /// keeps working while the user is mid-edit.
    pub fn reparse(&mut self) {
        let text = self.rope.to_string();
        self.parsed = parse_source_recovering_for_uri(&text, self.uri.as_str());
        // Refresh symbols/references whenever we have *any* body — including a
        // partially-recovered one. `compute_analysis` falls back to the
        // lenient text scan when the parse carried errors, so a transient
        // syntax error never drops declarations (which would cascade into
        // spurious "undefined variable" warnings on every reference).
        if self.parsed.body.is_some() {
            let (symbols, references) = compute_analysis(&self.parsed, &self.uri, &self.rope);
            self.symbols = symbols;
            self.references = references;
        }
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }
}

fn compute_analysis(parsed: &ParsedFile, uri: &Url, rope: &Rope) -> (SymbolTable, Vec<Reference>) {
    match &parsed.body {
        // Structured extraction only on a CLEAN parse. A partially-recovered
        // body (errors present) has had its broken lines blanked out, so
        // structured extraction would miss any declaration/reference that
        // lived on a blanked line — exactly the cascade the text fallback
        // exists to prevent. Use the fallback whenever the parse wasn't clean.
        Some(body) if !parsed.has_errors() => {
            let symbols = extract_symbols(body, uri, rope);
            let references = extract_references(body, uri, rope);
            (symbols, references)
        }
        _ => {
            // HCL parser bailed entirely — run the text-based
            // fallbacks so `variable "x" {}` declarations AND
            // `var.X` / `local.X` / `module.X` references around
            // the broken expression are still visible. Without
            // these, a single typo cascades into
            // "undefined variable" warnings on every reference in
            // the file AND "declared but not used" warnings on
            // every variable the file was consuming — because the
            // refs disappear from the workspace index until the
            // user fixes the parse error.
            let text = rope.to_string();
            let symbols = extract_symbols_fallback(&text, uri, rope);
            let references = extract_references_fallback(&text, uri, rope);
            (symbols, references)
        }
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
        let doc = DocumentState::new(test_uri(), r#"output "x" { value = var.region }"#, 1);
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

    /// A FULL-document-sync change: `range: None`, whole new text.
    fn full(text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }
    }

    // Under FULL sync each batch is the complete document, so an
    // out-of-order delivery just means a newer version may land first; the
    // older one must then be dropped rather than overwrite the newer text.
    #[test]
    fn versioned_changes_newer_full_text_wins_when_delivered_out_of_order() {
        let mut doc = DocumentState::new(test_uri(), "", 1);

        // v3 (the newer full document) lands first.
        let applied = doc
            .apply_versioned_changes(3, vec![full("hello\nworld\n")])
            .expect("apply");
        assert_eq!(applied, Some(3));
        assert_eq!(doc.text(), "hello\nworld\n");
        assert_eq!(doc.version, 3);

        // v2 (an older full document) arrives late — dropped, rope unchanged.
        let applied = doc
            .apply_versioned_changes(2, vec![full("STALE\n")])
            .expect("stale must not error");
        assert_eq!(applied, None);
        assert_eq!(doc.text(), "hello\nworld\n", "older version must not win");
        assert_eq!(doc.version, 3);
    }

    #[test]
    fn versioned_changes_in_order_apply_immediately() {
        let mut doc = DocumentState::new(test_uri(), "", 1);
        assert_eq!(
            doc.apply_versioned_changes(2, vec![full("a\n")])
                .expect("apply"),
            Some(2)
        );
        assert_eq!(
            doc.apply_versioned_changes(3, vec![full("a\nb\n")])
                .expect("apply"),
            Some(3)
        );
        assert_eq!(doc.text(), "a\nb\n");
        assert_eq!(doc.version, 3);
    }

    #[test]
    fn versioned_changes_stale_or_duplicate_ignored() {
        let mut doc = DocumentState::new(test_uri(), "", 1);
        doc.apply_versioned_changes(2, vec![full("x\n")])
            .expect("apply");
        // A duplicate of an already-applied version is a no-op.
        let applied = doc
            .apply_versioned_changes(2, vec![full("DUP")])
            .expect("stale must not error");
        assert_eq!(applied, None);
        assert_eq!(doc.text(), "x\n", "stale edit must not mutate the rope");
        assert_eq!(doc.version, 2);
    }
}
