//! Document lifecycle handlers: didOpen, didChange, didSave, didClose.
//!
//! Each handler updates the `StateStore` (which keeps the symbol and
//! reference indexes in sync) and publishes the union of all
//! diagnostic families back to the client.

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
    // Make sure the enclosing module directory has been indexed — the
    // file may be outside the original workspace root (e.g. opened by
    // Claude Code while editing an unrelated repo) and its sibling
    // definitions need to be in the store before diagnostics run.
    crate::indexer::ensure_module_indexed(&backend.state, &backend.jobs, &uri);
    publish_current_diagnostics(backend, &uri, None).await;
    // Kick off background version-cache prefetch so inlay-hint
    // freshness annotations (and the semantic no-match diagnostic)
    // light up without the user having to trigger completion first.
    crate::handlers::version_prefetch::spawn(backend, uri, None);
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
    // Re-prefetch in case the user added a new provider / module /
    // updated the Terraform required_version. Fresh caches are a no-op
    // inside the fetch functions so this is cheap when unchanged.
    crate::handlers::version_prefetch::spawn(backend, uri, None);
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
    let module_dir = crate::handlers::util::parent_dir(uri);
    out.extend(undefined_reference_diagnostics(&doc.references, |kind| {
        is_defined_in_module(state, module_dir.as_deref(), kind)
    }));

    if let Some(body) = doc.parsed.body.as_ref() {
        let lookup = StateStoreSchemaLookup { state };
        out.extend(resource_diagnostics(body, &doc.rope, uri, &lookup));
        let cache_lookup = OnDiskVersionCache;
        out.extend(tfls_diag::constraint_diagnostics(
            body,
            &doc.rope,
            &cache_lookup,
        ));
        out.extend(tfls_diag::variable_default_type_diagnostics(
            body, &doc.rope,
        ));
        // Tflint parity — in-tree "recommended" preset rules plus the
        // niche `module_shallow_clone`. Each walker is a single-file
        // HCL pass that reads the already-parsed `Body`, so the cost
        // is amortised across the work the document handler already
        // does.
        out.extend(tfls_diag::typed_variables_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::required_version_presence_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::required_providers_version_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::module_version_presence_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::module_pinned_source_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::module_shallow_clone_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::workspace_remote_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::deprecated_index_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::deprecated_interpolation_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::deprecated_lookup_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::empty_list_equality_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::map_duplicate_keys_diagnostics(body, &doc.rope));
    }

    out
}

/// Reads the already-populated on-disk caches used by the completion
/// path. Returning `None` suppresses the semantic no-match warning
/// (the completion fetch simply hasn't happened yet); returning a
/// `Vec<String>` lets `tfls-diag` compare user constraints against
/// actually-published versions.
struct OnDiskVersionCache;

impl tfls_diag::VersionCacheLookup for OnDiskVersionCache {
    fn cached_versions(
        &self,
        source: &tfls_diag::ConstraintSource,
    ) -> Option<Vec<String>> {
        match source {
            tfls_diag::ConstraintSource::TerraformCli => {
                // Cache directly under $XDG_CACHE_HOME/terraform-ls-rs/tool-versions/
                let path = tool_versions_cache_path("terraform")?;
                let tf = std::fs::read_to_string(&path).ok()?;
                let tofu_path = tool_versions_cache_path("opentofu")?;
                let tofu = std::fs::read_to_string(&tofu_path).ok();
                let mut out: Vec<String> = serde_json::from_str(&tf).ok()?;
                if let Some(tofu_body) = tofu {
                    if let Ok(extra) = serde_json::from_str::<Vec<String>>(&tofu_body) {
                        for v in extra {
                            if !out.contains(&v) {
                                out.push(v);
                            }
                        }
                    }
                }
                Some(out)
            }
            tfls_diag::ConstraintSource::Provider { namespace, name } => {
                let mut out = Vec::new();
                for registry in &["terraform", "opentofu"] {
                    let path = registry_versions_cache_path(registry, namespace, name)?;
                    if let Ok(body) = std::fs::read_to_string(&path) {
                        if let Ok(vs) = serde_json::from_str::<Vec<String>>(&body) {
                            for v in vs {
                                if !out.contains(&v) {
                                    out.push(v);
                                }
                            }
                        }
                    }
                }
                if out.is_empty() {
                    None
                } else {
                    Some(out)
                }
            }
            tfls_diag::ConstraintSource::Module {
                namespace,
                name,
                provider,
            } => {
                let mut out = Vec::new();
                for registry in &["terraform", "opentofu"] {
                    let path =
                        module_versions_cache_path(registry, namespace, name, provider)?;
                    if let Ok(body) = std::fs::read_to_string(&path) {
                        if let Ok(vs) = serde_json::from_str::<Vec<String>>(&body) {
                            for v in vs {
                                if !out.contains(&v) {
                                    out.push(v);
                                }
                            }
                        }
                    }
                }
                if out.is_empty() {
                    None
                } else {
                    Some(out)
                }
            }
        }
    }
}

fn cache_root_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("terraform-ls-rs"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            std::path::PathBuf::from(home)
                .join(".cache")
                .join("terraform-ls-rs"),
        );
    }
    None
}

fn sanitise(c: &str) -> String {
    c.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn tool_versions_cache_path(slug: &str) -> Option<std::path::PathBuf> {
    Some(
        cache_root_dir()?
            .join("tool-versions")
            .join(format!("{}.json", sanitise(slug))),
    )
}

fn registry_versions_cache_path(
    registry: &str,
    namespace: &str,
    name: &str,
) -> Option<std::path::PathBuf> {
    Some(
        cache_root_dir()?
            .join("registry-versions")
            .join(sanitise(registry))
            .join(sanitise(namespace))
            .join(sanitise(name))
            .join("versions.json"),
    )
}

fn module_versions_cache_path(
    registry: &str,
    namespace: &str,
    name: &str,
    provider: &str,
) -> Option<std::path::PathBuf> {
    Some(
        cache_root_dir()?
            .join("registry-versions")
            .join("modules")
            .join(sanitise(registry))
            .join(sanitise(namespace))
            .join(sanitise(name))
            .join(sanitise(provider))
            .join("versions.json"),
    )
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
    crate::handlers::util::parent_dir(&loc.uri).as_deref() == Some(dir)
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

