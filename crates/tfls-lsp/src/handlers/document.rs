//! Document lifecycle handlers: didOpen, didChange, didSave, didClose.
//!
//! Each handler updates the `StateStore` (which keeps the symbol and
//! reference indexes in sync) and publishes the union of all
//! diagnostic families back to the client.

use tfls_core::SymbolKind;
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
use crate::handlers::util::{
    module_supports_locals_replacement, module_supports_templatefile,
    module_supports_terraform_data,
};

pub async fn did_open(backend: &Backend, params: DidOpenTextDocumentParams) {
    let uri = params.text_document.uri.clone();
    backend.state.mark_open(uri.clone());
    let doc = DocumentState::new(
        uri.clone(),
        &params.text_document.text,
        params.text_document.version,
    );
    backend.state.upsert_document(doc);

    let action = did_open_publish_action(&backend.state);
    let need_diags = matches!(action, DidOpenPublish::PublishReal);

    // Move the heavy sync work off the tokio handler thread so
    // the runtime stays responsive to other requests (completion,
    // hover, other buffers' diagnostic pulls) while we index
    // peer files + compute this buffer's diagnostics. On a
    // module with 20 peer files, this otherwise pins a tokio
    // worker for 100ms-1s.
    let state = std::sync::Arc::clone(&backend.state);
    let jobs = std::sync::Arc::clone(&backend.jobs);
    let uri_c = uri.clone();
    let diagnostics = tokio::task::spawn_blocking(move || {
        // Make sure the enclosing module directory has been
        // indexed — the file may be outside the original
        // workspace root (e.g. opened by Claude Code while
        // editing an unrelated repo) and its sibling
        // definitions need to be in the store before
        // diagnostics run.
        crate::indexer::ensure_module_indexed(&state, &jobs, &uri_c);
        if need_diags {
            compute_diagnostics(&state, &uri_c)
        } else {
            Vec::new()
        }
    })
    .await
    .unwrap_or_default();

    // The buffer is now open. Hand off the diagnostic channel to
    // either (a) a one-time empty publish that clears whatever the
    // bulk workspace scan may have pushed to this URI before it
    // became an open buffer — followed by pull diagnostics taking
    // over — or (b) a normal push for clients that don't advertise
    // pull. `did_open_publish_action` is the single source of truth
    // for that choice; see its docs for the duplicate-diagnostic
    // invariant it pins.
    match action {
        DidOpenPublish::ClearPushNamespaceThenPull => {
            tracing::info!(
                uri = %uri,
                action = "ClearPushNamespaceThenPull",
                "did_open: publishing 0 diagnostics (clear push, pull takes over)",
            );
            // Empty `publishDiagnostics` resets the push namespace.
            // Subsequent pulls populate the (separate) pull
            // namespace; nvim's display is pull-only for this URI.
            backend
                .client
                .publish_diagnostics(uri.clone(), Vec::new(), None)
                .await;
        }
        DidOpenPublish::PublishReal => {
            tracing::info!(
                uri = %uri,
                action = "PublishReal",
                count = diagnostics.len(),
                "did_open: publishing diagnostics",
            );
            backend
                .client
                .publish_diagnostics(uri.clone(), diagnostics, None)
                .await;
        }
    }

    // Kick off background version-cache prefetch so inlay-hint
    // freshness annotations (and the semantic no-match diagnostic)
    // light up without the user having to trigger completion first.
    crate::handlers::version_prefetch::spawn(backend, uri, None);
}

/// What the server should publish to the client on `did_open`.
/// Factored out of the async handler so the no-duplicate invariant
/// below is unit-testable without mocking the LSP client.
///
/// Critical invariant: under pull-diagnostics mode the server must
/// reset the push namespace to empty BEFORE pull takes over.
/// Background scans (`indexer::scan_files_parallel`) publish
/// diagnostics for every indexed file, which is correct for files
/// the user never opens (workspace-wide views consume the push
/// namespace). But once the user DOES open a file, nvim displays
/// the union of push + pull as two separate diagnostic lists —
/// the pre-open push entries become stale or duplicated.
/// `ClearPushNamespaceThenPull` emits one empty publish that
/// resets the namespace; after that nvim shows pull-only for the
/// buffer's lifetime.
///
/// `PublishReal` is the push-only path for clients that never
/// advertised pull — we still need to tell them about diagnostics
/// somehow, and there's no double-namespace concern to mitigate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DidOpenPublish {
    /// Client advertised pull. Clear the push namespace with one
    /// empty `publishDiagnostics`; subsequent pulls populate the
    /// pull namespace. Total lifetime push count for this URI
    /// under pull mode: exactly 1 (the empty clear).
    ///
    /// Currently unreachable — `did_open_publish_action` forces
    /// `PublishReal` because the server does not advertise
    /// `diagnostic_provider`. Variant retained for the if/when we
    /// re-enable pull. Marked `#[allow(dead_code)]` until then.
    #[allow(dead_code)]
    ClearPushNamespaceThenPull,
    /// Client didn't advertise pull. Compute + push real
    /// diagnostics the normal way.
    PublishReal,
}

pub(crate) fn did_open_publish_action(_state: &StateStore) -> DidOpenPublish {
    // ALWAYS push. The server's `capabilities.diagnostic_provider`
    // is `None` (see `capabilities.rs`), so no client will ever
    // pull from us, regardless of whether the client itself
    // advertises pull support. Returning `ClearPushNamespaceThenPull`
    // based on the CLIENT's capability — without considering
    // whether THIS server actually serves pull — emits an empty
    // publishDiagnostics and then waits for a pull that never
    // arrives. Net effect: every client (e.g. nvim, which always
    // advertises `textDocument.diagnostic`) sees zero diagnostics
    // forever.
    //
    // If/when we re-enable `diagnostic_provider`, restore the
    // capability check here.
    DidOpenPublish::PublishReal
}

pub async fn did_change(backend: &Backend, params: DidChangeTextDocumentParams) {
    let uri = params.text_document.uri.clone();
    let version = params.text_document.version;
    tracing::info!(uri = %uri, version, "did_change");

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

    // Reparse + diagnostic compute are both CPU-heavy; hand
    // them to a blocking thread so the tokio runtime stays
    // responsive to concurrent requests on other buffers.
    let state = std::sync::Arc::clone(&backend.state);
    let uri_c = uri.clone();
    let _ = tokio::task::spawn_blocking(move || {
        state.reparse_document(&uri_c);
    })
    .await;
    publish_current_diagnostics(backend, &uri, Some(version)).await;
    // Changes to THIS file can invalidate diagnostics in OTHER
    // open buffers in the same module. Push fresh diagnostics
    // directly to each such open peer; this is the reliable
    // signal across nvim versions.
    //
    // We deliberately do NOT also send
    // `workspace/diagnostic/refresh` here: in nvim 0.11+ the
    // refresh handler invalidates the pull-diagnostic namespace
    // for every buffer it tracks, which can race our subsequent
    // push (the push lands on an "abandoned" namespace and the
    // display stays stale). Relying on the push alone keeps the
    // update path single-source — every observed staleness bug
    // has been a refresh-then-push race, never a
    // push-didn't-land.
    publish_peer_diagnostics(backend, &uri).await;
}

pub async fn did_save(backend: &Backend, params: DidSaveTextDocumentParams) {
    let uri = params.text_document.uri;
    tracing::info!(uri = %uri, "did_save");
    // Same as did_change — off to a blocking thread.
    let state = std::sync::Arc::clone(&backend.state);
    let uri_c = uri.clone();
    let _ = tokio::task::spawn_blocking(move || {
        state.reparse_document(&uri_c);
    })
    .await;
    publish_current_diagnostics(backend, &uri, None).await;
    // See did_change: push peer-buffer diagnostics directly
    // instead of relying on workspace/diagnostic/refresh, which
    // races our own push inside nvim 0.11+.
    publish_peer_diagnostics(backend, &uri).await;
    // Re-check the `.terraform/providers/` tree — if the user ran
    // `tofu init` / `terraform init` since we last fetched (adding
    // or upgrading a provider), the mtime will have bumped and
    // `refresh_schemas_if_providers_changed` enqueues a fresh
    // FetchSchemas so search / hover / completion pick up the
    // newly-installed provider.
    crate::indexer::refresh_schemas_if_providers_changed(
        &backend.state,
        &backend.jobs,
        &uri,
    );
    // Re-prefetch in case the user added a new provider / module /
    // updated the Terraform required_version. Fresh caches are a no-op
    // inside the fetch functions so this is cheap when unchanged.
    crate::handlers::version_prefetch::spawn(backend, uri, None);
}

pub async fn did_close(backend: &Backend, params: DidCloseTextDocumentParams) {
    let uri = params.text_document.uri;
    backend.state.mark_closed(&uri);
    backend.state.remove_document(&uri);
    // Always clear on close — symmetric with `did_open`'s
    // pull-mode clear. Ensures the push namespace is empty when
    // the buffer stops being an active editor target, so the next
    // `did_open` starts from a known-clean state.
    backend
        .client
        .publish_diagnostics(uri, Vec::new(), None)
        .await;
}

/// Push fresh diagnostics to every OPEN peer buffer in the same
/// module directory as `changed_uri`.
///
/// Used after `did_change` / `did_save` to clear cross-file
/// invalidations (typically "undefined variable" / "declared but
/// not used") that go stale when a declaration in one `.tf` is
/// added / removed while a reference lives in a peer. The spec-
/// correct `workspace/diagnostic/refresh` signal is already sent
/// in `did_change` / `did_save`, but real-world clients (nvim
/// 0.11+ in particular) don't always re-pull for buffers that
/// aren't currently visible, so the display stays stale until the
/// next edit in the affected buffer. A direct push clears the
/// namespace immediately; a later re-pull (if the client does
/// honour the refresh) overwrites with identical data.
///
/// Bypasses `should_skip_push_diagnostics` on purpose: the goal
/// here is exactly the cross-file refresh that the skip rule
/// otherwise defers to pull-mode. Only peer buffers (not
/// `changed_uri` itself — `publish_current_diagnostics` covers
/// that) get the push.
async fn publish_peer_diagnostics(backend: &Backend, changed_uri: &Url) {
    let Some(module_dir) = crate::handlers::util::parent_dir(changed_uri) else {
        return;
    };

    let peers: Vec<Url> = backend
        .state
        .documents
        .iter()
        .filter_map(|entry| {
            let uri = entry.key();
            if uri == changed_uri {
                return None;
            }
            if !backend.state.is_open(uri) {
                return None;
            }
            let parent = crate::handlers::util::parent_dir(uri)?;
            if parent != module_dir {
                return None;
            }
            Some(uri.clone())
        })
        .collect();

    tracing::info!(
        changed = %changed_uri,
        module_dir = %module_dir.display(),
        peer_count = peers.len(),
        "publish_peer_diagnostics: selected peers"
    );

    if peers.is_empty() {
        return;
    }

    let state = std::sync::Arc::clone(&backend.state);
    let peers_for_compute = peers.clone();
    let results: Vec<(Url, Vec<Diagnostic>)> = tokio::task::spawn_blocking(move || {
        peers_for_compute
            .into_iter()
            .map(|uri| {
                let diagnostics = compute_diagnostics(&state, &uri);
                (uri, diagnostics)
            })
            .collect()
    })
    .await
    .unwrap_or_default();

    for (uri, diagnostics) in results {
        tracing::info!(
            uri = %uri,
            n = diagnostics.len(),
            "publish_peer_diagnostics: push (version=None — unconditional apply)"
        );
        // Send without a version so the client treats the publish as
        // unconditional. Some clients (nvim 0.11 in particular) drop
        // a publish whose version equals the one they already have
        // for the buffer — the stored version on this peer doc is
        // the last edit WE saw, not the one the client has, so
        // sending it is worse than useless.
        backend.client.publish_diagnostics(uri, diagnostics, None).await;
    }
}

async fn publish_current_diagnostics(backend: &Backend, uri: &Url, version: Option<i32>) {
    // When the client negotiated pull diagnostics at initialize time,
    // pushing for an open buffer would duplicate the same issue in
    // the client's store (nvim tracks push + pull in separate
    // namespaces). Skip push; client will pull on demand. For
    // unopened workspace files we still push so `:Trouble
    // workspace_diagnostics` etc. populate.
    if backend.state.should_skip_push_diagnostics(uri) {
        return;
    }
    // Compute on a blocking thread so the tokio worker stays
    // free for other handlers; `compute_diagnostics` can burn
    // hundreds of ms on a large file + module graph.
    let state = std::sync::Arc::clone(&backend.state);
    let uri_c = uri.clone();
    let diagnostics = tokio::task::spawn_blocking(move || compute_diagnostics(&state, &uri_c))
        .await
        .unwrap_or_default();
    backend
        .client
        .publish_diagnostics(uri.clone(), diagnostics, version)
        .await;
}

/// Compute the full diagnostic set for a document: syntax errors,
/// undefined-reference warnings, and schema validation errors.
///
/// Builds a fresh [`ModuleGraphAdapter`] per call — fine for
/// single-doc edits but O(N²) when called for every file in a
/// workspace scan. The bulk-scan path uses
/// [`compute_diagnostics_with_lookup`] with a precomputed snapshot
/// instead.
pub fn compute_diagnostics(state: &StateStore, uri: &Url) -> Vec<Diagnostic> {
    let module_dir = crate::handlers::util::parent_dir(uri);
    let graph = ModuleGraphAdapter {
        state,
        module_dir: module_dir.as_deref(),
        current_uri: uri,
    };
    let current_file = uri
        .path_segments()
        .and_then(|mut it| it.next_back())
        .unwrap_or("")
        .to_string();
    compute_diagnostics_with_lookup(state, uri, &graph, &current_file)
}

/// Same as [`compute_diagnostics`] but takes an injected
/// [`tfls_diag::ModuleGraphLookup`]. Lets the bulk-scan path reuse a
/// cached [`crate::handlers::module_snapshot::ModuleSnapshot`]
/// across every URI in a module instead of rebuilding the aggregates
/// per file.
pub fn compute_diagnostics_with_lookup(
    state: &StateStore,
    uri: &Url,
    graph: &dyn tfls_diag::ModuleGraphLookup,
    current_file: &str,
) -> Vec<Diagnostic> {
    let Some(doc) = state.documents.get(uri) else {
        return Vec::new();
    };

    let mut out = diagnostics_for_parse_errors(&doc.parsed.errors);

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
        // Pass the module-graph lookup so typed-variables can
        // suppress its warning on variables that are ALSO
        // unused — fixing the type on a soon-to-be-deleted
        // variable wastes the user's time. Lookup is only
        // consulted on root modules, matching
        // `unused_declarations`'s own gating.
        out.extend(tfls_diag::typed_variables_diagnostics(
            body,
            &doc.rope,
            Some(graph),
        ));
        out.extend(tfls_diag::module_version_presence_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::module_pinned_source_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::module_shallow_clone_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::workspace_remote_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::deprecated_index_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::deprecated_interpolation_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::deprecated_lookup_diagnostics(body, &doc.rope));
        // Module-aware gating: a `terraform { required_version }`
        // block typically lives in `versions.tf`, not the file we're
        // scanning, so we aggregate every sibling's constraint before
        // deciding whether to flag `null_resource` / `template_file`
        // blocks here.
        let null_resource_supported = module_supports_terraform_data(state, uri);
        out.extend(tfls_diag::deprecated_null_resource_diagnostics_for_module(
            body,
            &doc.rope,
            null_resource_supported,
        ));
        let templatefile_supported = module_supports_templatefile(state, uri);
        out.extend(tfls_diag::deprecated_template_file_diagnostics_for_module(
            body,
            &doc.rope,
            templatefile_supported,
        ));
        out.extend(tfls_diag::deprecated_template_dir_diagnostics_for_module(
            body,
            &doc.rope,
            templatefile_supported,
        ));
        let locals_supported = module_supports_locals_replacement(state, uri);
        out.extend(tfls_diag::deprecated_null_data_source_diagnostics_for_module(
            body,
            &doc.rope,
            locals_supported,
        ));
        out.extend(tfls_diag::empty_list_equality_diagnostics(body, &doc.rope));
        out.extend(tfls_diag::map_duplicate_keys_diagnostics(body, &doc.rope));

        // Cross-file / module-scoped rules. `graph` is either the
        // fresh per-call adapter (from `compute_diagnostics`) or a
        // cached snapshot (from the bulk-scan path).
        out.extend(tfls_diag::required_version_presence_diagnostics(
            body, &doc.rope, graph,
        ));
        out.extend(tfls_diag::required_providers_version_diagnostics(
            body, &doc.rope, graph,
        ));
        out.extend(tfls_diag::unused_declarations_diagnostics(
            body, &doc.rope, graph,
        ));
        out.extend(tfls_diag::unused_required_providers_diagnostics(
            body, &doc.rope, graph,
        ));
        out.extend(tfls_diag::standard_module_structure_diagnostics(
            body,
            &doc.rope,
            current_file,
            graph,
        ));

        // Pass 3 — opt-in style pack.
        if state.config.snapshot().style_rules {
            out.extend(tfls_diag::documented_variables_diagnostics(body, &doc.rope));
            out.extend(tfls_diag::documented_outputs_diagnostics(body, &doc.rope));
            out.extend(tfls_diag::naming_convention_diagnostics(body, &doc.rope));
            out.extend(tfls_diag::comment_syntax_diagnostics(&doc.rope));
        }
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
    locs.iter()
        .any(|loc| crate::handlers::util::location_in_dir(loc, module_dir))
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

/// Adapter that answers the Pass 2 cross-file questions by reading
/// [`StateStore`]. Keyed on the document's own module directory so
/// references from *other* modules in the same workspace don't
/// mask an unused declaration here.
struct ModuleGraphAdapter<'a> {
    state: &'a StateStore,
    module_dir: Option<&'a std::path::Path>,
    current_uri: &'a Url,
}

impl ModuleGraphAdapter<'_> {
    fn has_ref(&self, key: &SymbolKey) -> bool {
        let Some(locs) = self.state.references_by_name.get(key) else {
            return false;
        };
        match self.module_dir {
            Some(dir) => locs
                .iter()
                .any(|loc| crate::handlers::util::location_in_dir(loc, dir)),
            None => !locs.is_empty(),
        }
    }
}

impl tfls_diag::ModuleGraphLookup for ModuleGraphAdapter<'_> {
    fn variable_is_referenced(&self, name: &str) -> bool {
        self.has_ref(&SymbolKey::new(SymbolKind::Variable, name))
    }

    fn local_is_referenced(&self, name: &str) -> bool {
        self.has_ref(&SymbolKey::new(SymbolKind::Local, name))
    }

    fn data_source_is_referenced(&self, type_name: &str, name: &str) -> bool {
        self.has_ref(&SymbolKey::resource(SymbolKind::DataSource, type_name, name))
    }

    fn used_provider_locals(&self) -> std::collections::HashSet<String> {
        // Provider local names are the prefix of resource types
        // (`aws_instance` → `aws`) plus any explicit local used via
        // `provider = foo.alias`. Walk every parsed document in the
        // same module dir to collect them.
        let mut used = std::collections::HashSet::new();
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            if let Some(dir) = self.module_dir {
                let doc_dir = crate::handlers::util::parent_dir(doc.key());
                if doc_dir.as_deref() != Some(dir) {
                    continue;
                }
            }
            collect_provider_locals(body, &mut used);
        }
        used
    }

    fn present_files(&self) -> std::collections::HashSet<String> {
        let Some(dir) = self.module_dir else {
            return std::collections::HashSet::new();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return std::collections::HashSet::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.to_string())
                    .filter(|s| s.ends_with(".tf") || s.ends_with(".tf.json"))
            })
            .collect()
    }

    fn is_primary_terraform_doc(&self) -> bool {
        // Primary = lexicographically-first URI in the same module
        // that contains at least one top-level `terraform {}` block.
        let mut candidates: Vec<String> = Vec::new();
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            if let Some(dir) = self.module_dir {
                let doc_dir = crate::handlers::util::parent_dir(doc.key());
                if doc_dir.as_deref() != Some(dir) {
                    continue;
                }
            }
            let has_tf_block = body
                .iter()
                .any(|s| s.as_block().is_some_and(|b| b.ident.as_str() == "terraform"));
            if has_tf_block {
                candidates.push(doc.key().as_str().to_string());
            }
        }
        candidates.sort();
        candidates
            .first()
            .map(|s| s.as_str() == self.current_uri.as_str())
            .unwrap_or(false)
    }

    fn module_has_required_version(&self) -> bool {
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            if let Some(dir) = self.module_dir {
                let doc_dir = crate::handlers::util::parent_dir(doc.key());
                if doc_dir.as_deref() != Some(dir) {
                    continue;
                }
            }
            for structure in body.iter() {
                let Some(block) = structure.as_block() else {
                    continue;
                };
                if block.ident.as_str() != "terraform" {
                    continue;
                }
                if block.body.iter().any(|s| {
                    s.as_attribute()
                        .is_some_and(|a| a.key.as_str() == "required_version")
                }) {
                    return true;
                }
            }
        }
        false
    }

    fn providers_with_version_set(&self) -> std::collections::HashSet<String> {
        use hcl_edit::expr::{Expression, ObjectKey};
        let mut out = std::collections::HashSet::new();
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            if let Some(dir) = self.module_dir {
                let doc_dir = crate::handlers::util::parent_dir(doc.key());
                if doc_dir.as_deref() != Some(dir) {
                    continue;
                }
            }
            for structure in body.iter() {
                let Some(tf_block) = structure.as_block() else {
                    continue;
                };
                if tf_block.ident.as_str() != "terraform" {
                    continue;
                }
                for inner in tf_block.body.iter() {
                    let Some(rp_block) = inner.as_block() else {
                        continue;
                    };
                    if rp_block.ident.as_str() != "required_providers" {
                        continue;
                    }
                    for entry in rp_block.body.iter() {
                        let Some(attr) = entry.as_attribute() else {
                            continue;
                        };
                        let name = attr.key.as_str();
                        let Expression::Object(obj) = &attr.value else {
                            continue;
                        };
                        let has_version = obj.iter().any(|(k, _v)| match k {
                            ObjectKey::Ident(id) => id.as_str() == "version",
                            ObjectKey::Expression(Expression::Variable(v)) => {
                                v.value().as_str() == "version"
                            }
                            ObjectKey::Expression(Expression::String(s)) => {
                                s.value().as_str() == "version"
                            }
                            _ => false,
                        });
                        if has_version {
                            out.insert(name.to_string());
                        }
                    }
                }
            }
        }
        out
    }

    fn is_root_module(&self) -> bool {
        // We're a root module if no `module { source = "..." }`
        // block in any other module resolves to our directory.
        // Cheap heuristic: check whether any indexed document's
        // body has a `module` block whose resolved source points
        // at our dir. Exact path resolution is handled elsewhere;
        // here we accept any hit as "not root" to keep the check
        // conservative.
        let Some(dir) = self.module_dir else {
            // Without a dir we can't tell — assume root (the user
            // probably opened a lone file).
            return true;
        };
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            let doc_dir = crate::handlers::util::parent_dir(doc.key());
            // Skip documents in the same module — a module calling
            // itself isn't a concern, and intra-module `module`
            // blocks point at sub-dirs, not this dir.
            if doc_dir.as_deref() == Some(dir) {
                continue;
            }
            for structure in body.iter() {
                let Some(block) = structure.as_block() else {
                    continue;
                };
                if block.ident.as_str() != "module" {
                    continue;
                }
                for attr in block.body.iter().filter_map(|s| s.as_attribute()) {
                    if attr.key.as_str() != "source" {
                        continue;
                    }
                    if let hcl_edit::expr::Expression::String(s) = &attr.value {
                        let src = s.value().as_str();
                        if source_points_at(src, doc_dir.as_deref(), dir) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }
}

/// Resolve a module `source = "..."` string relative to the calling
/// module's dir and check whether it points at `target`. Only
/// local-path sources are resolved; everything else (registry, git,
/// etc.) can't possibly point at a local workspace dir.
fn source_points_at(
    source: &str,
    caller_dir: Option<&std::path::Path>,
    target: &std::path::Path,
) -> bool {
    if !(source.starts_with("./") || source.starts_with("../") || source.starts_with('/')) {
        return false;
    }
    let Some(caller_dir) = caller_dir else {
        return false;
    };
    let resolved = caller_dir.join(source);
    // Normalise both paths for comparison.
    let resolved = match std::fs::canonicalize(&resolved) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let target = match std::fs::canonicalize(target) {
        Ok(p) => p,
        Err(_) => return false,
    };
    resolved == target
}

/// Walk a body collecting every provider local name used by
/// `resource`/`data` blocks (via resource-type prefix) and by
/// explicit `provider = foo.alias` attrs.
fn collect_provider_locals(
    body: &hcl_edit::structure::Body,
    out: &mut std::collections::HashSet<String>,
) {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        match block.ident.as_str() {
            "resource" | "data" => {
                if let Some(label) = block.labels.first() {
                    let type_name = match label {
                        hcl_edit::structure::BlockLabel::String(s) => s.value().as_str(),
                        hcl_edit::structure::BlockLabel::Ident(i) => i.as_str(),
                    };
                    if let Some(local) = type_name.split('_').next() {
                        if !local.is_empty() {
                            out.insert(local.to_string());
                        }
                    }
                }
                // `provider = foo.alias` inside the block body.
                for attr in block.body.iter().filter_map(|s| s.as_attribute()) {
                    if attr.key.as_str() == "provider" {
                        if let Some(local) = extract_provider_local(&attr.value) {
                            out.insert(local);
                        }
                    }
                }
            }
            "provider" => {
                if let Some(label) = block.labels.first() {
                    let name = match label {
                        hcl_edit::structure::BlockLabel::String(s) => {
                            s.value().as_str().to_string()
                        }
                        hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
                    };
                    out.insert(name);
                }
            }
            _ => {}
        }
    }
}

/// Extract `foo` from a `provider = foo.alias` expression.
fn extract_provider_local(expr: &hcl_edit::expr::Expression) -> Option<String> {
    match expr {
        hcl_edit::expr::Expression::Variable(v) => Some(v.value().as_str().to_string()),
        hcl_edit::expr::Expression::Traversal(t) => {
            if let hcl_edit::expr::Expression::Variable(v) = &t.expr {
                Some(v.value().as_str().to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod did_open_publish_tests {
    //! Invariant tests for the `did_open` publish-action
    //! decision.
    //!
    //! **The invariant:** under pull-diagnostics mode the first
    //! publish for a freshly-opened buffer MUST be an empty set,
    //! not a real diagnostic payload. Background workspace scans
    //! push real diagnostics to files BEFORE they're open — those
    //! entries live in nvim's push namespace. Once the buffer is
    //! open, pull takes over and populates a SEPARATE pull
    //! namespace; nvim's display is the union of the two. Unless
    //! we clear the push namespace on did_open, stale or
    //! duplicate diagnostics show up for every edit session.
    //!
    //! These tests pin `did_open_publish_action`'s output so a
    //! future commit can't silently revert the clear to a real
    //! publish (the bug we've regressed into multiple times).

    use super::{DidOpenPublish, did_open_publish_action};
    use tfls_state::StateStore;

    #[test]
    fn always_publish_real_while_pull_unadvertised() {
        // Server doesn't advertise `diagnostic_provider`, so push
        // is the only mode. Either client capability flag must
        // produce `PublishReal`.
        let store = StateStore::new();
        store.set_client_supports_pull_diagnostics(true);
        assert_eq!(
            did_open_publish_action(&store),
            DidOpenPublish::PublishReal
        );
        let store = StateStore::new();
        assert_eq!(
            did_open_publish_action(&store),
            DidOpenPublish::PublishReal
        );
    }

    #[test]
    fn action_enum_has_no_push_real_under_pull_variant() {
        // Meta-invariant — parallel to the `RefreshDecision`
        // enum's equivalent test. The two legitimate actions on
        // did_open are "clear then pull" and "publish real", no
        // others. Adding a third — e.g. "publish real then also
        // refresh" — would reintroduce the duplicate-diagnostic
        // regression. Match exhaustively so a future commit
        // can't add a variant without a source-level change.
        let variants = [
            DidOpenPublish::ClearPushNamespaceThenPull,
            DidOpenPublish::PublishReal,
        ];
        for v in variants {
            match v {
                DidOpenPublish::ClearPushNamespaceThenPull
                | DidOpenPublish::PublishReal => {}
            }
        }
    }
}
