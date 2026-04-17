//! Navigation handlers: goto-definition, find-references, hover.

use lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams, Location,
    MarkupContent, MarkupKind, ReferenceParams,
};
use tfls_core::SymbolKind;
use tfls_state::{SymbolKey, reference_at_position, reference_key};
use tower_lsp::jsonrpc;

use crate::backend::Backend;

/// `textDocument/declaration` — for HCL this is identical to
/// `textDocument/definition`. Clients often call both, so we expose
/// the same behaviour under a separate name rather than returning
/// `None` for declaration requests.
pub async fn goto_declaration(
    backend: &Backend,
    params: lsp_types::request::GotoDeclarationParams,
) -> jsonrpc::Result<Option<lsp_types::request::GotoDeclarationResponse>> {
    goto_definition(backend, params).await
}

pub async fn goto_definition(
    backend: &Backend,
    params: GotoDefinitionParams,
) -> jsonrpc::Result<Option<GotoDefinitionResponse>> {
    let pos = params.text_document_position_params.position;
    let uri = params.text_document_position_params.text_document.uri;

    let key = match backend.state.documents.get(&uri) {
        Some(doc) => reference_at_position(&doc, pos).map(|r| reference_key(&r.kind)),
        None => None,
    };
    let Some(key) = key else {
        return Ok(None);
    };

    let locations: Vec<Location> = backend
        .state
        .definitions_by_name
        .get(&key)
        .map(|entry| entry.iter().map(|l| l.to_lsp_location()).collect())
        .unwrap_or_default();

    if locations.is_empty() {
        Ok(None)
    } else {
        // LSP accepts Array for any count; avoid the single-item Scalar case
        // to keep the handler branch-free.
        Ok(Some(GotoDefinitionResponse::Array(locations)))
    }
}

pub async fn references(
    backend: &Backend,
    params: ReferenceParams,
) -> jsonrpc::Result<Option<Vec<Location>>> {
    let pos = params.text_document_position.position;
    let uri = params.text_document_position.text_document.uri;

    // Key lookup — works whether the cursor is on a definition or a reference.
    let key = match backend.state.documents.get(&uri) {
        Some(doc) => key_at_cursor(&doc, pos),
        None => None,
    };
    let Some(key) = key else {
        return Ok(None);
    };

    let mut out: Vec<Location> = Vec::new();
    if params.context.include_declaration {
        if let Some(entry) = backend.state.definitions_by_name.get(&key) {
            out.extend(entry.iter().map(|l| l.to_lsp_location()));
        }
    }
    if let Some(entry) = backend.state.references_by_name.get(&key) {
        out.extend(entry.iter().map(|l| l.to_lsp_location()));
    }

    if out.is_empty() { Ok(None) } else { Ok(Some(out)) }
}

pub async fn hover(backend: &Backend, params: HoverParams) -> jsonrpc::Result<Option<Hover>> {
    let pos = params.text_document_position_params.position;
    let uri = params.text_document_position_params.text_document.uri;

    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let Some(reference) = reference_at_position(&doc, pos) else {
        return Ok(None);
    };
    let key = reference_key(&reference.kind);
    let detail = describe_key(&key);

    Ok(Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: detail,
        }),
        range: Some(reference.location.range()),
    }))
}

fn key_at_cursor(doc: &tfls_state::DocumentState, pos: lsp_types::Position) -> Option<SymbolKey> {
    // If cursor is on a reference, use its key.
    if let Some(r) = reference_at_position(doc, pos) {
        return Some(reference_key(&r.kind));
    }
    // Otherwise look for a defining symbol whose range contains the cursor.
    for (name, sym) in &doc.symbols.variables {
        if contains(&sym.location.range(), pos) {
            return Some(SymbolKey::new(SymbolKind::Variable, name));
        }
    }
    for (name, sym) in &doc.symbols.locals {
        if contains(&sym.location.range(), pos) {
            return Some(SymbolKey::new(SymbolKind::Local, name));
        }
    }
    for (name, sym) in &doc.symbols.outputs {
        if contains(&sym.location.range(), pos) {
            return Some(SymbolKey::new(SymbolKind::Output, name));
        }
    }
    for (name, sym) in &doc.symbols.modules {
        if contains(&sym.location.range(), pos) {
            return Some(SymbolKey::new(SymbolKind::Module, name));
        }
    }
    for (addr, sym) in &doc.symbols.resources {
        if contains(&sym.location.range(), pos) {
            return Some(SymbolKey::resource(
                SymbolKind::Resource,
                &addr.resource_type,
                &addr.name,
            ));
        }
    }
    for (addr, sym) in &doc.symbols.data_sources {
        if contains(&sym.location.range(), pos) {
            return Some(SymbolKey::resource(
                SymbolKind::DataSource,
                &addr.resource_type,
                &addr.name,
            ));
        }
    }
    None
}

fn contains(range: &lsp_types::Range, pos: lsp_types::Position) -> bool {
    let after_start = (pos.line, pos.character) >= (range.start.line, range.start.character);
    let before_end = (pos.line, pos.character) <= (range.end.line, range.end.character);
    after_start && before_end
}

fn describe_key(key: &SymbolKey) -> String {
    let kind = match key.kind {
        SymbolKind::Variable => "variable",
        SymbolKind::Local => "local",
        SymbolKind::Output => "output",
        SymbolKind::Module => "module",
        SymbolKind::Resource => "resource",
        SymbolKind::DataSource => "data source",
        SymbolKind::Provider => "provider",
        SymbolKind::TerraformBlock => "terraform block",
    };
    format!("**{kind}** `{name}`", name = key.name)
}

