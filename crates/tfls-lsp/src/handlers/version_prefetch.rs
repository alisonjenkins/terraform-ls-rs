//! Background version-cache prefetch.
//!
//! Without this, a fresh file can sit with visible version constraints
//! whose freshness hints never render because the on-disk cache was
//! never populated by a completion interaction. We solve that by
//! walking the document on open / save, enumerating every constraint
//! target (Terraform CLI, provider, module), and kicking off the same
//! `fetch_*` APIs the completion path uses — all in the background so
//! the main document handler stays responsive. When fetches finish we
//! ask the client to re-request inlay hints (and we re-publish
//! diagnostics so the semantic no-match warning lights up too).
//!
//! The actual "figure out what to fetch, fetch it" core lives in
//! `tfls_engine::prefetch` — this module is a thin LSP adapter: it
//! extracts targets for the single document being prefetched (with
//! the pre-fetch `is_cached` short-circuit still applied here so a
//! warm-cache `did_change` stays a true no-op without even calling
//! into the engine), wires a `WarmProgress` impl over
//! `ProgressReporter`, calls `tfls_engine::prefetch::warm_caches`,
//! then does the LSP-only tail (inlay-hint refresh + diagnostic
//! republish) that `tfls-lint` has no equivalent of.

use std::sync::Arc;

use tfls_engine::prefetch::{warm_caches, NoopProgress, WarmOptions, WarmProgress, WarmTarget};
use tfls_state::StateStore;
use url::Url;

use crate::backend::Backend;
use crate::progress::ProgressReporter;

/// Fire-and-forget: parse the document, fetch every uncached version
/// target in parallel, then trigger client-side inlay-hint refresh +
/// diagnostic re-publish when the last fetch completes.
pub fn spawn(backend: &Backend, uri: Url, version: Option<i32>) {
    let state = Arc::clone(&backend.state);
    let client = backend.client.clone();
    tokio::spawn(async move {
        prefetch_and_refresh(state, client, uri, version).await;
    });
}

/// Fire-and-forget eager fetch of Terraform / OpenTofu CLI release
/// catalogues AND the common-provider version catalogues at server
/// startup. Empty / un-initialised workspaces don't go through the
/// per-document `did_open` path with anything meaningful to fetch —
/// `collect_targets` returns empty until the user types something —
/// so the on-disk cache stays cold and the first user-visible
/// interaction (typing a `required_version` constraint, accepting a
/// `required_providers` block-snippet for a fresh provider, hovering
/// over an existing version) ends up paying the full registry
/// round-trip.
///
/// Doing this once on `initialize` populates the cache before the
/// first keystroke, so completion / hover / inlay-hints / the
/// "no matching version" diagnostic all light up immediately on
/// the very first `did_change`. The provider-catalog prefetch in
/// particular makes the `required_providers_entry_items`
/// snippet bake a real `~> MAJOR.MINOR` from cache instead of
/// falling back to an empty tabstop on cold start.
///
/// All HTTP fetches respect the existing 24h disk cache; subsequent
/// server starts are network-free. This delegates to the engine's
/// `warm_caches` (with `NoopProgress` — a single eager batch at
/// startup doesn't need a `$/progress` stream) rather than the
/// per-document collector, since there's no document yet.
pub fn spawn_eager_tool_versions(client: tower_lsp_server::Client) {
    tokio::spawn(async move {
        let mut targets = vec![WarmTarget::TerraformCli];
        for (_, source, _) in tfls_core::builtin_blocks::REQUIRED_PROVIDERS_COMMON_ENTRIES {
            let Some((ns, name)) = source.split_once('/') else {
                continue;
            };
            targets.push(WarmTarget::Provider {
                namespace: ns.to_string(),
                name: name.to_string(),
            });
        }

        let opts = WarmOptions { cli_enabled: true };
        let _ = warm_caches(&targets, &opts, &NoopProgress).await;

        // Refresh inlay hints so the freshness annotations light
        // up against the now-warm cache. Failure to refresh is
        // non-fatal — clients that don't advertise the capability
        // just won't refresh until the next user-driven request.
        let _ = crate::progress::bounded_request(
            "workspace/inlayHint/refresh",
            client.inlay_hint_refresh(),
        )
        .await;
    });
}

/// Adapts a `ProgressReporter`'s sync-friendly `ReportSender` to the
/// engine's `WarmProgress` trait. `WarmProgress::report` is
/// deliberately sync (the engine has no dependency on tokio-flavoured
/// async plumbing), so this uses `send_detached` — order relative to
/// the reporter's eventual `end()` is preserved because every sender
/// shares the same drain queue.
struct ProgressReporterAdapter {
    sender: crate::progress::ReportSender,
}

impl WarmProgress for ProgressReporterAdapter {
    fn report(&self, done: usize, total: usize, message: &str) {
        let percentage = if total == 0 {
            None
        } else {
            Some(((done as f64 / total as f64) * 100.0) as u32)
        };
        self.sender
            .send_detached(Some(message.to_string()), percentage);
    }
}

async fn prefetch_and_refresh(
    state: Arc<StateStore>,
    client: tower_lsp_server::Client,
    uri: Url,
    _version: Option<i32>,
) {
    let targets = match state.documents.get(&uri) {
        Some(doc) => match doc.parsed.body.as_ref() {
            Some(body) => tfls_engine::prefetch::collect_targets_from_body(body),
            None => return,
        },
        None => return,
    };
    if targets.is_empty() {
        return;
    }

    // Filter to targets whose cache file is missing on disk.
    // `did_change` fires this prefetch on every keystroke once
    // wired in (see `did_change` handler); without the filter,
    // every keystroke would surface a "Fetching N version
    // catalog(s)" progress dialog and an `inlay_hint_refresh` /
    // diagnostic re-publish, even though the actual `fetch_*`
    // calls inside short-circuit on the 24h disk cache. Filter
    // up front so warm-cache `did_change` is a true no-op —
    // avoids even calling into `warm_caches` on the hot path.
    let targets: Vec<WarmTarget> = targets.into_iter().filter(|t| !t.is_cached()).collect();
    if targets.is_empty() {
        return;
    }

    // Git tag-list resolution shells out to `git`; honor the cliEnabled gate.
    let cli_enabled = state.config.snapshot().cli_enabled;
    let opts = WarmOptions { cli_enabled };

    // User-visible progress for the batch. Individual fetches run
    // concurrently so we can't report "provider 3/10" meaningfully
    // — just show the set of targets at begin time.
    let progress = ProgressReporter::begin(
        &client,
        &state,
        format!("Fetching {} version catalog(s)", targets.len()),
    )
    .await;

    let report = match &progress {
        Some(p) => {
            let adapter = ProgressReporterAdapter { sender: p.sender() };
            warm_caches(&targets, &opts, &adapter).await
        }
        None => warm_caches(&targets, &opts, &NoopProgress).await,
    };
    for (target, err) in &report.failed {
        tracing::debug!(target = %target, error = %err, "version prefetch fetch failed");
    }

    if let Some(p) = progress {
        p.end(Some("version catalogs ready".to_string())).await;
    }

    // Ask the client to re-request inlay hints. The standard LSP
    // method is `workspace/inlayHint/refresh`; tower-lsp exposes it
    // on `Client::inlay_hint_refresh`. We ignore failures — an older
    // client that doesn't support the capability just won't refresh
    // until the next user action.
    let _ = crate::progress::bounded_request(
        "workspace/inlayHint/refresh",
        client.inlay_hint_refresh(),
    )
    .await;

    // Also refresh diagnostics so the semantic no-match warning
    // (fired by `constraint_diagnostics` when the version constraint
    // resolves to zero published versions in the cache) re-evaluates
    // against the freshly-fetched version list. Without this, the
    // warning would only appear on the next user-triggered edit,
    // leaving the file apparently clean even when the constraint
    // is actually unsatisfiable.
    crate::indexer::maybe_refresh_diagnostics(&state, Some(&client)).await;
}
