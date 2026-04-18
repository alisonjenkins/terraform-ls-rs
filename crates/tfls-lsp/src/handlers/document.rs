//! Document lifecycle handlers: didOpen, didChange, didSave, didClose.
//!
//! Each handler updates the `StateStore` (which keeps the symbol and
//! reference indexes in sync) and publishes the union of all
//! diagnostic families back to the client.

use std::path::PathBuf;

use tfls_core::{SymbolKind, SymbolLocation};
use tfls_diag::{
    diagnostics_for_parse_errors, resource_diagnostics, undefined_reference_diagnostics,
};
use tfls_parser::ReferenceKind;
use tfls_schema::Schema;
use tfls_state::{DocumentState, StateStore, SymbolKey};
use tower_lsp::lsp_types::{
    Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, MessageType, Url,
};

use crate::backend::Backend;

pub async fn did_open(backend: &Backend, params: DidOpenTextDocumentParams) {
    let uri = params.text_document.uri.clone();
    let doc = DocumentState::new(
        uri.clone(),
        &params.text_document.text,
        params.text_document.version,
    );
    backend.state.upsert_document(doc);
    publish_current_diagnostics(backend, &uri, None).await;
}

pub async fn did_change(backend: &Backend, params: DidChangeTextDocumentParams) {
    let uri = params.text_document.uri.clone();
    let version = params.text_document.version;

    let apply_err = {
        let mut entry = match backend.state.documents.get_mut(&uri) {
            Some(e) => e,
            None => {
                tracing::warn!(uri = %uri, "didChange for unknown document");
                return;
            }
        };
        entry.version = version;
        let mut err = None;
        for change in params.content_changes {
            if let Err(e) = entry.apply_change(change) {
                err = Some(e);
                break;
            }
        }
        err
    };

    if let Some(e) = apply_err {
        backend
            .client
            .log_message(MessageType::ERROR, format!("edit apply failed: {e}"))
            .await;
        return;
    }

    backend.state.reparse_document(&uri);
    publish_current_diagnostics(backend, &uri, Some(version)).await;
}

pub async fn did_save(backend: &Backend, params: DidSaveTextDocumentParams) {
    let uri = params.text_document.uri;
    backend.state.reparse_document(&uri);
    publish_current_diagnostics(backend, &uri, None).await;
}

pub async fn did_close(backend: &Backend, params: DidCloseTextDocumentParams) {
    let uri = params.text_document.uri;
    backend.state.remove_document(&uri);
    backend
        .client
        .publish_diagnostics(uri, Vec::new(), None)
        .await;
}

async fn publish_current_diagnostics(backend: &Backend, uri: &Url, version: Option<i32>) {
    let diagnostics = compute_diagnostics(&backend.state, uri);
    backend
        .client
        .publish_diagnostics(uri.clone(), diagnostics, version)
        .await;
}

/// Compute the full diagnostic set for a document: syntax errors,
/// undefined-reference warnings, and schema validation errors.
pub fn compute_diagnostics(state: &StateStore, uri: &Url) -> Vec<Diagnostic> {
    let Some(doc) = state.documents.get(uri) else {
        return Vec::new();
    };

    let mut out = diagnostics_for_parse_errors(&doc.parsed.errors);

    // Undefined-reference resolution scoped to the referencing document's
    // parent directory — a Terraform module is one directory, so a reference
    // in `<dir>/a.tf` is satisfied by any definition in `<dir>/*.tf` but not
    // by definitions inside `<dir>/modules/**` or unrelated workspace roots.
    let module_dir = parent_dir(uri);
    out.extend(undefined_reference_diagnostics(&doc.references, |kind| {
        is_defined_in_module(state, module_dir.as_deref(), kind)
    }));

    if let Some(body) = doc.parsed.body.as_ref() {
        let lookup = StateStoreSchemaLookup { state };
        out.extend(resource_diagnostics(body, &doc.rope, uri, &lookup));
    }

    out
}

/// True if a definition for `kind` exists somewhere in the workspace index
/// with the same parent directory as the referencing document. Falls back to
/// a lenient `true` for URIs we can't resolve to a filesystem path, so
/// nonsense `file://` inputs don't spam diagnostics.
fn is_defined_in_module(
    state: &StateStore,
    module_dir: Option<&std::path::Path>,
    kind: &ReferenceKind,
) -> bool {
    let key = match kind {
        ReferenceKind::Variable { name } => SymbolKey::new(SymbolKind::Variable, name),
        ReferenceKind::Local { name } => SymbolKey::new(SymbolKind::Local, name),
        ReferenceKind::Module { name } => SymbolKey::new(SymbolKind::Module, name),
        // resource / data-source refs are skipped upstream by the diag engine.
        _ => return true,
    };
    let Some(locs) = state.definitions_by_name.get(&key) else {
        return false;
    };
    let Some(module_dir) = module_dir else {
        // Without a parseable parent dir we can't compare; treat as defined
        // to avoid false positives on exotic URIs.
        return !locs.is_empty();
    };
    locs.iter().any(|loc| location_in_dir(loc, module_dir))
}

fn location_in_dir(loc: &SymbolLocation, dir: &std::path::Path) -> bool {
    parent_dir(&loc.uri).as_deref() == Some(dir)
}

fn parent_dir(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()?.parent().map(|p| p.to_path_buf())
}

/// Adapter so `tfls-diag` can query [`StateStore`]-installed schemas
/// via its [`tfls_diag::schema_validation::SchemaLookup`] trait.
struct StateStoreSchemaLookup<'a> {
    state: &'a StateStore,
}

impl tfls_diag::schema_validation::SchemaLookup for StateStoreSchemaLookup<'_> {
    fn resource(&self, type_name: &str) -> Option<Schema> {
        self.state.resource_schema(type_name)
    }
    fn data_source(&self, type_name: &str) -> Option<Schema> {
        self.state.data_source_schema(type_name)
    }
}

