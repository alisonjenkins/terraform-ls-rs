//! Document lifecycle handlers: didOpen, didChange, didSave, didClose.
//!
//! Each handler updates the `StateStore` (which keeps the symbol and
//! reference indexes in sync) and publishes the union of all
//! diagnostic families back to the client.

use lsp_types::{
    Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, MessageType,
};
use tfls_parser::ReferenceKind;
use tfls_state::{DocumentState, StateStore};
use url::Url;

use crate::backend::Backend;

pub async fn did_open(backend: &Backend, params: DidOpenTextDocumentParams) {
    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document.uri) else {
        return;
    };
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
                .publish_diagnostics(tfls_core::uri::url_to_uri(&uri), Vec::new(), None)
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
                .publish_diagnostics(tfls_core::uri::url_to_uri(&uri), diagnostics, None)
                .await;
        }
    }

    // A freshly-opened file can DEFINE symbols (variables, locals, outputs,
    // resources, …) that other already-open files in the same module
    // reference. Those consumers were computed before this file's
    // definitions entered the index, so they may be showing a stale
    // `undefined …` for a symbol this file provides. Refresh open peers now.
    // (did_change already does this for edits; did_open didn't, so opening
    // the file that defines a referenced variable never cleared the
    // consumer until the consumer itself was edited.)
    publish_peer_diagnostics(backend, &uri).await;

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

/// A hash of everything in `doc` that can affect ANOTHER file's
/// diagnostics: its definitions (var/local/output/module/resource/data),
/// its references, and the raw text of its `terraform {}` blocks
/// (required_version / required_providers). Used to skip the
/// recompute-all-open-peers pass when an edit (a value, comment, or
/// whitespace change) leaves cross-file state untouched.
fn cross_file_fingerprint(doc: &DocumentState) -> u64 {
    use hcl_edit::repr::Span as _;
    use std::hash::{Hash, Hasher};

    let mut tokens: Vec<String> = Vec::new();
    let s = &doc.symbols;
    tokens.extend(s.variables.keys().map(|k| format!("v:{k}")));
    tokens.extend(s.locals.keys().map(|k| format!("l:{k}")));
    tokens.extend(s.outputs.keys().map(|k| format!("o:{k}")));
    tokens.extend(s.modules.keys().map(|k| format!("m:{k}")));
    tokens.extend(
        s.resources
            .keys()
            .map(|a| format!("r:{}.{}", a.resource_type, a.name)),
    );
    tokens.extend(
        s.data_sources
            .keys()
            .map(|a| format!("d:{}.{}", a.resource_type, a.name)),
    );
    for r in &doc.references {
        tokens.push(match &r.kind {
            ReferenceKind::Variable { name } => format!("ref:var.{name}"),
            ReferenceKind::Local { name } => format!("ref:local.{name}"),
            ReferenceKind::Module { name } => format!("ref:module.{name}"),
            ReferenceKind::Resource {
                resource_type,
                name,
            } => {
                format!("ref:{resource_type}.{name}")
            }
            ReferenceKind::DataSource {
                resource_type,
                name,
            } => {
                format!("ref:data.{resource_type}.{name}")
            }
        });
    }
    // Raw `terraform {}` block text — captures required_version /
    // required_providers edits that change peer version diagnostics.
    if let Some(body) = doc.parsed.body.as_ref() {
        for st in body.iter() {
            let Some(b) = st.as_block() else { continue };
            if b.ident.as_str() != "terraform" {
                continue;
            }
            if let Some(span) = b.span() {
                let text = doc.rope.byte_slice(span.start..span.end).to_string();
                tokens.push(format!("tf:{text}"));
            }
        }
    }

    tokens.sort();
    let mut h = rustc_hash::FxHasher::default();
    for t in &tokens {
        t.hash(&mut h);
    }
    h.finish()
}

pub async fn did_change(backend: &Backend, params: DidChangeTextDocumentParams) {
    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document.uri) else {
        return;
    };
    let version = params.text_document.version;
    tracing::info!(uri = %uri, version, "did_change");

    // Fingerprint the doc's cross-file-relevant state BEFORE the edit, so
    // we can skip the recompute-all-peers pass when it didn't change.
    let old_fingerprint = backend
        .state
        .documents
        .get(&uri)
        .map(|d| cross_file_fingerprint(&d));

    // Apply the edit AND reparse atomically, under the per-document lock.
    // FULL sync means each batch is the whole document; `apply_and_reparse_
    // document` advances rope + parsed + symbols under a SINGLE store guard,
    // so no concurrent reader (hover/completion/diagnostics) ever sees a
    // torn new-rope/old-AST state, and a stale/duplicate version is dropped.
    // The doc lock is held across the (CPU-heavy, blocking-threaded) work so
    // an edit-emitting request like `textDocument/formatting` waits for the
    // apply instead of formatting stale text and reverting the edit.
    let lock = backend.doc_lock(&uri);
    let result = {
        let _guard = lock.lock().await;
        let state = std::sync::Arc::clone(&backend.state);
        let uri_c = uri.clone();
        let changes = params.content_changes;
        tokio::task::spawn_blocking(move || {
            state.apply_and_reparse_document(&uri_c, version, changes)
        })
        .await
    };
    let applied = match result {
        Ok(Ok(Some(v))) => v,
        Ok(Ok(None)) => {
            // Stale/duplicate version — rope unchanged; the handler that
            // applied the newer version publishes the up-to-date result.
            tracing::debug!(uri = %uri, version, "did_change: stale version dropped");
            return;
        }
        Ok(Err(e)) => {
            backend
                .client
                .log_message(MessageType::ERROR, format!("edit apply failed: {e}"))
                .await;
            return;
        }
        Err(join) => {
            tracing::error!(uri = %uri, error = %join, "did_change: apply task failed");
            return;
        }
    };

    // Did THIS edit change the doc's cross-file-relevant state (declared /
    // referenced names, the `terraform {}` block)? Computed before the
    // coalescing check because BOTH paths below need it.
    let new_fingerprint = backend
        .state
        .documents
        .get(&uri)
        .map(|d| cross_file_fingerprint(&d));
    let cross_file_changed = match (old_fingerprint, new_fingerprint) {
        (Some(a), Some(b)) => a != b,
        _ => true,
    };

    // In-flight coalescing: tower-lsp runs notification handlers
    // concurrently, so fast typing can overlap several did_change tasks
    // for the same buffer. If a newer edit has already landed, this one is
    // stale — skip its OWN-file compute/publish; the newer handler produces
    // the up-to-date result. But we must NOT skip the peer pass when this
    // edit changed cross-file state: the newer handler captured its
    // `old_fingerprint` AFTER our edit applied, so it can't see our change
    // and would skip its own peer pass — leaving a consumer file stuck with
    // a stale undefined-reference for a symbol this edit just declared.
    let superseded = backend
        .state
        .documents
        .get(&uri)
        .is_some_and(|d| d.version != applied);
    if superseded {
        tracing::debug!(uri = %uri, applied, "did_change: superseded by a newer edit");
        if cross_file_changed {
            publish_peer_diagnostics(backend, &uri).await;
        }
        return;
    }

    publish_current_diagnostics(backend, &uri, Some(applied)).await;
    // Re-run the version-cache prefetch in case this edit
    // introduced a new constraint target (typed `required_version`
    // for the first time, added a new provider, swapped a module
    // source). The prefetch filters to uncached targets up front,
    // so warm-cache keystrokes are a true no-op (no progress
    // dialog, no refresh churn). Lets a user starting a fresh
    // file see completion / inlay-hints / no-match diagnostics
    // immediately after the first relevant keystroke instead of
    // waiting for the next did_save.
    crate::handlers::version_prefetch::spawn(backend, uri.clone(), Some(applied));
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
    //
    // Skip the (O(open peers) × full-module-compute) pass when this edit
    // left the doc's cross-file state untouched — a value, comment, or
    // whitespace change can't affect any peer's diagnostics.
    if cross_file_changed {
        publish_peer_diagnostics(backend, &uri).await;
    } else {
        tracing::debug!(uri = %uri, "did_change: cross-file state unchanged; skipping peer recompute");
    }
}

pub async fn did_save(backend: &Backend, params: DidSaveTextDocumentParams) {
    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document.uri) else {
        return;
    };
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
    crate::indexer::refresh_schemas_if_providers_changed(&backend.state, &backend.jobs, &uri);
    // Re-prefetch in case the user added a new provider / module /
    // updated the Terraform required_version. Fresh caches are a no-op
    // inside the fetch functions so this is cheap when unchanged.
    crate::handlers::version_prefetch::spawn(backend, uri, None);
}

pub async fn did_close(backend: &Backend, params: DidCloseTextDocumentParams) {
    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document.uri) else {
        return;
    };
    backend.state.mark_closed(&uri);
    backend.state.remove_document(&uri);
    // Always clear on close — symmetric with `did_open`'s
    // pull-mode clear. Ensures the push namespace is empty when
    // the buffer stops being an active editor target, so the next
    // `did_open` starts from a known-clean state.
    backend
        .client
        .publish_diagnostics(tfls_core::uri::url_to_uri(&uri), Vec::new(), None)
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
pub(crate) async fn publish_peer_diagnostics(backend: &Backend, changed_uri: &Url) {
    // Peers share `changed_uri`'s MODULE SCOPE, not just its parent dir: a
    // `.tftest.hcl` in `tests/` resolves its `var.X` / `output.X` against
    // the module one dir up, so editing that module's `.tf` must refresh
    // the open test file (and vice-versa). `open_peers_in_scope` pairs them
    // via `module_scope_dir`.
    let peers: Vec<Url> = crate::handlers::util::open_peers_in_scope(&backend.state, changed_uri);

    tracing::info!(
        changed = %changed_uri,
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
        backend
            .client
            .publish_diagnostics(tfls_core::uri::url_to_uri(&uri), diagnostics, None)
            .await;
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
        .publish_diagnostics(tfls_core::uri::url_to_uri(uri), diagnostics, version)
        .await;
}

/// The diagnostics pipeline itself — moved to `tfls-engine` so the
/// (future) standalone lint CLI can reuse it without pulling in
/// tower-lsp. Re-exported here so every existing
/// `tfls_lsp::handlers::document::compute_diagnostics[_with_lookup]`
/// call site keeps compiling unchanged.
pub use tfls_engine::pipeline::{compute_diagnostics, compute_diagnostics_with_lookup};

/// Re-export so `indexer.rs`'s `StateStoreSchemaLookup { state }`
/// construction keeps resolving through this module.
pub(crate) use tfls_engine::module::StateStoreSchemaLookup;

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

    use super::{did_open_publish_action, DidOpenPublish};
    use tfls_state::StateStore;

    #[test]
    fn always_publish_real_while_pull_unadvertised() {
        // Server doesn't advertise `diagnostic_provider`, so push
        // is the only mode. Either client capability flag must
        // produce `PublishReal`.
        let store = StateStore::new();
        store.set_client_supports_pull_diagnostics(true);
        assert_eq!(did_open_publish_action(&store), DidOpenPublish::PublishReal);
        let store = StateStore::new();
        assert_eq!(did_open_publish_action(&store), DidOpenPublish::PublishReal);
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
                DidOpenPublish::ClearPushNamespaceThenPull | DidOpenPublish::PublishReal => {}
            }
        }
    }
}
