//! Navigation handlers: goto-definition, find-references, hover.

use lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams, Location,
    MarkupContent, MarkupKind, Position, ReferenceParams, Url,
};
use tfls_core::SymbolKind;
use tfls_parser::lsp_position_to_byte_offset;
use tfls_state::{DocumentState, SymbolKey, reference_at_position, reference_key};
use tower_lsp::jsonrpc;

use crate::backend::Backend;
use crate::handlers::cursor::{find_symbol_at_cursor, key_at_cursor};
use crate::handlers::util::{lookup_child_module_symbol, parent_dir, resolve_module_source};
use crate::handlers::{hover_attribute, hover_function, hover_module_input};

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

    // Module-scoped goto-def — handled BEFORE the generic
    // `reference_at_position` path because those positions (an
    // attribute key inside a module block, the output-name segment
    // of `module.foo.bar`) are not modelled as cross-block
    // references in the symbol index. Both descend *into* the child
    // module's source, so they run first and fall through on miss.
    if let Some(doc) = backend.state.documents.get(&uri) {
        if let Some(loc) = module_input_goto_at(&backend.state, doc.value(), &uri, pos) {
            return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
        }
    }

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

/// Goto-def target for a cursor sitting on an attribute key inside a
/// `module "LABEL" { KEY = … }` block. Returns the LSP location of
/// the `variable "KEY" { }` declaration in the child module — or
/// `None` if the cursor isn't on such a key, if the child module's
/// source can't be resolved to a directory, or if the child module
/// doesn't declare a variable with that name.
fn module_input_goto_at(
    state: &tfls_state::StateStore,
    doc: &DocumentState,
    uri: &Url,
    pos: Position,
) -> Option<Location> {
    let body = doc.parsed.body.as_ref()?;
    let offset = lsp_position_to_byte_offset(&doc.rope, pos).ok()?;

    let module_block = hover_module_input::find_module_block_at(body, offset)?;
    let module_label = module_block
        .labels
        .first()
        .and_then(hover_module_input::label_str)?;
    let source = hover_module_input::string_attribute(module_block, "source")?;

    // Cursor must be on an attribute key inside the module body.
    let (attr_name, _range) =
        hover_module_input::attribute_key_at(&module_block.body, offset, &doc.rope)?;
    // Don't intercept the `source` / `version` / `providers` /
    // `count` / `for_each` / `depends_on` keys themselves — those
    // aren't user-declared inputs of the child module.
    if matches!(
        attr_name.as_str(),
        "source" | "version" | "providers" | "count" | "for_each" | "depends_on"
    ) {
        return None;
    }

    let parent = parent_dir(uri)?;
    let child = resolve_module_source(&parent, &module_label, &source)?;

    lookup_child_module_symbol(state, &child, SymbolKind::Variable, &attr_name)
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

    // Prefer the narrowest match: attribute hover beats symbol hover when the
    // cursor is on an attribute key inside a resource body, because the
    // resource's symbol range contains the attribute's position too.
    if let Some(hover) = hover_attribute::attribute_hover(&backend.state, &doc, pos, &uri) {
        return Ok(Some(hover));
    }

    // Module-input hover: a key inside a `module "…" {}` block points
    // at the referenced child module's variable declaration.
    if let Some(hover) = hover_module_input::module_input_hover(&backend.state, &doc, pos) {
        return Ok(Some(hover));
    }

    // Function calls come before symbol hover: function names share their
    // span with nothing in the symbol tables, but the enclosing output /
    // resource would otherwise "win" and produce a useless hover.
    if let Some(hover) = hover_function::function_hover(&backend.state, &doc, pos) {
        return Ok(Some(hover));
    }

    // Fall back to symbol under cursor (reference OR defining block label).
    if let Some(target) = find_symbol_at_cursor(&doc, pos) {
        let detail = describe_key(&target.key);
        return Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: detail,
            }),
            range: Some(target.location.range()),
        }));
    }

    Ok(None)
}

pub(crate) fn describe_key(key: &SymbolKey) -> String {
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
