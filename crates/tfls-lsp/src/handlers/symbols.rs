//! `textDocument/documentSymbol` — outline view.
//! `workspace/symbol` — workspace-wide fuzzy symbol search.

use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, SymbolInformation, SymbolKind,
    WorkspaceSymbolParams,
};
use tfls_core::{Symbol, SymbolKind as DomainKind};
use tfls_state::DocumentState;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub async fn document_symbol(
    backend: &Backend,
    params: DocumentSymbolParams,
) -> jsonrpc::Result<Option<DocumentSymbolResponse>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };

    let symbols = collect_document_symbols(&doc);
    if symbols.is_empty() {
        Ok(None)
    } else {
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }
}

fn collect_document_symbols(doc: &DocumentState) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    for sym in doc.symbols.variables.values() {
        out.push(to_document_symbol(sym));
    }
    for sym in doc.symbols.locals.values() {
        out.push(to_document_symbol(sym));
    }
    for sym in doc.symbols.outputs.values() {
        out.push(to_document_symbol(sym));
    }
    for sym in doc.symbols.resources.values() {
        out.push(to_document_symbol(sym));
    }
    for sym in doc.symbols.data_sources.values() {
        out.push(to_document_symbol(sym));
    }
    for sym in doc.symbols.modules.values() {
        out.push(to_document_symbol(sym));
    }
    for sym in doc.symbols.providers.values() {
        out.push(to_document_symbol(sym));
    }
    out.sort_by(|a, b| {
        (a.range.start.line, a.range.start.character)
            .cmp(&(b.range.start.line, b.range.start.character))
    });
    out
}

#[allow(deprecated)]
fn to_document_symbol(sym: &Symbol) -> DocumentSymbol {
    let range = sym.location.range();
    DocumentSymbol {
        name: sym.name.clone(),
        detail: sym.detail.clone(),
        kind: lsp_symbol_kind(sym.kind),
        tags: None,
        deprecated: None,
        range,
        // selection_range is the identifier name; we don't track it
        // separately yet, so use the full range.
        selection_range: range,
        children: None,
    }
}

fn lsp_symbol_kind(k: DomainKind) -> SymbolKind {
    match k {
        DomainKind::Variable => SymbolKind::VARIABLE,
        DomainKind::Local => SymbolKind::VARIABLE,
        DomainKind::Output => SymbolKind::FIELD,
        DomainKind::Resource => SymbolKind::CLASS,
        DomainKind::DataSource => SymbolKind::CLASS,
        DomainKind::Module => SymbolKind::MODULE,
        DomainKind::Provider => SymbolKind::INTERFACE,
        DomainKind::TerraformBlock => SymbolKind::NAMESPACE,
    }
}

pub async fn workspace_symbol(
    backend: &Backend,
    params: WorkspaceSymbolParams,
) -> jsonrpc::Result<Option<Vec<SymbolInformation>>> {
    let query = params.query.to_ascii_lowercase();
    let mut results: Vec<SymbolInformation> = Vec::new();

    // Walk each document's per-file symbols; they own richer metadata
    // than the global index (which only knows locations).
    for doc_entry in backend.state.documents.iter() {
        let doc = doc_entry.value();
        collect_matching(&doc.symbols.variables, &query, &mut results);
        collect_matching(&doc.symbols.locals, &query, &mut results);
        collect_matching(&doc.symbols.outputs, &query, &mut results);
        collect_matching_resource(&doc.symbols.resources, &query, &mut results);
        collect_matching_resource(&doc.symbols.data_sources, &query, &mut results);
        collect_matching(&doc.symbols.modules, &query, &mut results);
        collect_matching(&doc.symbols.providers, &query, &mut results);
    }

    if results.is_empty() {
        Ok(None)
    } else {
        Ok(Some(results))
    }
}

fn collect_matching<K>(
    map: &std::collections::HashMap<K, Symbol>,
    query: &str,
    out: &mut Vec<SymbolInformation>,
) {
    for sym in map.values() {
        if matches_query(&sym.name, query) {
            out.push(to_symbol_information(sym));
        }
    }
}

fn collect_matching_resource(
    map: &std::collections::HashMap<tfls_core::ResourceAddress, Symbol>,
    query: &str,
    out: &mut Vec<SymbolInformation>,
) {
    for (addr, sym) in map {
        let full = format!("{addr}");
        if matches_query(&sym.name, query) || matches_query(&full, query) {
            out.push(to_symbol_information(sym));
        }
    }
}

/// Simple subsequence match on ASCII-lowercase so `awsinst` hits
/// `aws_instance`. Case-insensitive.
fn matches_query(name: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let haystack = name.to_ascii_lowercase();
    let mut q = query.chars();
    let mut current = q.next();
    for c in haystack.chars() {
        match current {
            Some(needle) if needle == c => current = q.next(),
            Some(_) => {}
            None => return true,
        }
    }
    current.is_none()
}

#[allow(deprecated)]
fn to_symbol_information(sym: &Symbol) -> SymbolInformation {
    SymbolInformation {
        name: sym.name.clone(),
        kind: lsp_symbol_kind(sym.kind),
        tags: None,
        deprecated: None,
        location: sym.location.to_lsp_location(),
        container_name: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn matches_empty_query_matches_all() {
        assert!(matches_query("aws_instance", ""));
    }

    #[test]
    fn matches_subsequence_case_insensitive() {
        assert!(matches_query("aws_instance", "awsinst"));
        assert!(matches_query("AWS_Instance", "awsinst"));
        assert!(matches_query("aws_instance", "ai"));
    }

    #[test]
    fn matches_rejects_wrong_order() {
        assert!(!matches_query("aws_instance", "instanceaws"));
    }

    #[test]
    fn matches_rejects_missing_chars() {
        assert!(!matches_query("aws_instance", "xyz"));
    }
}
