//! `textDocument/documentHighlight` — highlight every occurrence of
//! the symbol under the cursor within the current document.
//!
//! Uses the existing reference + definition data, filtered to the
//! current URI. Definitions are tagged `Write`, references `Read`.

use lsp_types::{DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams};
use tfls_state::{reference_at_position, reference_key};
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub async fn document_highlight(
    backend: &Backend,
    params: DocumentHighlightParams,
) -> jsonrpc::Result<Option<Vec<DocumentHighlight>>> {
    let uri = params
        .text_document_position_params
        .text_document
        .uri
        .clone();
    let pos = params.text_document_position_params.position;

    let key = {
        let Some(doc) = backend.state.documents.get(&uri) else {
            return Ok(None);
        };
        let Some(reference) = reference_at_position(&doc, pos) else {
            return Ok(None);
        };
        reference_key(&reference.kind)
    };

    let mut out = Vec::new();

    if let Some(defs) = backend.state.definitions_by_name.get(&key) {
        for loc in defs.iter().filter(|l| l.uri == uri) {
            out.push(DocumentHighlight {
                range: loc.range(),
                kind: Some(DocumentHighlightKind::WRITE),
            });
        }
    }
    if let Some(refs) = backend.state.references_by_name.get(&key) {
        for loc in refs.iter().filter(|l| l.uri == uri) {
            out.push(DocumentHighlight {
                range: loc.range(),
                kind: Some(DocumentHighlightKind::READ),
            });
        }
    }

    if out.is_empty() { Ok(None) } else { Ok(Some(out)) }
}
