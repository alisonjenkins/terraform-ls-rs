//! `textDocument/documentSymbol` — outline view.
//! `workspace/symbol` — workspace-wide fuzzy symbol search.

use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, SymbolInformation, SymbolKind,
    WorkspaceSymbolParams,
};
use tfls_core::{ResourceAddress, Symbol, SymbolKind as DomainKind, SymbolVisitor};
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
    struct Collector(Vec<DocumentSymbol>);
    impl SymbolVisitor for Collector {
        fn visit(&mut self, sym: &Symbol) {
            self.0.push(to_document_symbol(sym));
        }
        // Default `visit_resource` falls through to `visit` —
        // document_symbol doesn't need the address, just the
        // symbol itself.
    }
    let mut c = Collector(Vec::new());
    doc.symbols.for_each_symbol(&mut c);
    let mut out = c.0;
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
    struct Matcher<'a> {
        query: &'a str,
        out: &'a mut Vec<SymbolInformation>,
    }
    impl<'a> SymbolVisitor for Matcher<'a> {
        fn visit(&mut self, sym: &Symbol) {
            if matches_query(&sym.name, self.query) {
                self.out.push(to_symbol_information(sym));
            }
        }
        fn visit_resource(&mut self, addr: &ResourceAddress, sym: &Symbol) {
            // Resource search matches on either the bare name
            // or the full `type.name` identity so a query like
            // `aws_instance.web` resolves.
            let full = format!("{addr}");
            if matches_query(&sym.name, self.query) || matches_query(&full, self.query) {
                self.out.push(to_symbol_information(sym));
            }
        }
    }
    for doc_entry in backend.state.documents.iter() {
        let doc = doc_entry.value();
        let mut m = Matcher {
            query: &query,
            out: &mut results,
        };
        doc.symbols.for_each_symbol(&mut m);
        // Provider-defined function calls (Terraform 1.8+).
        // Surfaced via a text scan so workspace search can hit
        // `provider::aws::trim_prefix` — these calls aren't part
        // of the symbol table (they're expression-position
        // function calls, not declarations), but matching them
        // gives the user a workspace-wide grep affordance.
        collect_provider_function_calls(
            doc_entry.key(),
            doc,
            &query,
            &mut results,
        );
    }

    if results.is_empty() {
        Ok(None)
    } else {
        Ok(Some(results))
    }
}

#[allow(deprecated)]
fn collect_provider_function_calls(
    uri: &lsp_types::Url,
    doc: &tfls_state::DocumentState,
    query: &str,
    out: &mut Vec<SymbolInformation>,
) {
    let text = doc.rope.to_string();
    let bytes = text.as_bytes();
    let needle = b"provider::";
    let mut search_from = 0usize;
    while search_from + needle.len() <= bytes.len() {
        let Some(rel) = bytes[search_from..]
            .windows(needle.len())
            .position(|w| w == needle)
        else {
            break;
        };
        let kw_start = search_from + rel;
        if kw_start > 0 {
            let prev = bytes[kw_start - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                search_from = kw_start + needle.len();
                continue;
            }
        }
        let mut p = kw_start + needle.len();
        let local_start = p;
        while p < bytes.len() && (bytes[p].is_ascii_alphanumeric() || bytes[p] == b'_') {
            p += 1;
        }
        if p == local_start
            || p + 1 >= bytes.len()
            || bytes[p] != b':'
            || bytes[p + 1] != b':'
        {
            search_from = p;
            continue;
        }
        let fn_start = p + 2;
        let mut q = fn_start;
        while q < bytes.len() && (bytes[q].is_ascii_alphanumeric() || bytes[q] == b'_') {
            q += 1;
        }
        if q == fn_start {
            search_from = q;
            continue;
        }
        let mut r = q;
        while r < bytes.len() && (bytes[r] == b' ' || bytes[r] == b'\t') {
            r += 1;
        }
        if r >= bytes.len() || bytes[r] != b'(' {
            search_from = q;
            continue;
        }
        let name = &text[kw_start..q];
        if matches_query(name, query) {
            if let (Ok(start), Ok(end)) = (
                tfls_parser::byte_offset_to_lsp_position(&doc.rope, kw_start),
                tfls_parser::byte_offset_to_lsp_position(&doc.rope, q),
            ) {
                out.push(SymbolInformation {
                    name: name.to_string(),
                    kind: lsp_types::SymbolKind::FUNCTION,
                    tags: None,
                    deprecated: None,
                    location: lsp_types::Location {
                        uri: uri.clone(),
                        range: lsp_types::Range { start, end },
                    },
                    container_name: None,
                });
            }
        }
        search_from = q;
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
