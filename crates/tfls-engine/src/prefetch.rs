//! Transport-free cache warming shared by the LSP server's background
//! prefetch (`tfls-lsp`'s `handlers::version_prefetch`) and `tfls-lint`.
//!
//! Several diagnostics (module git-ref checks, Terraform/OpenTofu CLI
//! and provider/module registry version constraints) read on-disk
//! caches under `$XDG_CACHE_HOME/terraform-ls-rs/` rather than fetching
//! over the network inline — the fetch is expensive and the LSP has
//! historically warmed the cache in the background so the first
//! `did_open` diagnostics pass sees a cold cache and the next one
//! (after prefetch completes) sees a warm one. `tfls-lint` runs once
//! and exits, so it needs the warm-then-lint order collapsed into a
//! single synchronous step. This module holds the pure "walk a
//! `StateStore`, figure out what to fetch, fetch it" core with no
//! dependency on `tower_lsp_server`/`Backend`/`Client` or `tokio`
//! directly, so both callers can drive it from their own runtime.

use std::collections::HashSet;

use hcl_edit::expr::Expression;
use hcl_edit::structure::Body;
use tfls_state::StateStore;

/// One thing whose version/ref catalogue can be cached on disk.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum WarmTarget {
    TerraformCli,
    Provider {
        namespace: String,
        name: String,
    },
    Module {
        namespace: String,
        name: String,
        provider: String,
    },
    /// A git module repo whose tag list powers the mutable-ref / mismatch
    /// / outdated diagnostics. `url` is the normalized clonable URL
    /// (deduped across modules sharing a repo, e.g. a monorepo's subdir
    /// modules).
    GitRepo {
        url: String,
    },
}

impl WarmTarget {
    /// Stable human-readable identifier for reports/logs.
    pub fn label(&self) -> String {
        match self {
            WarmTarget::TerraformCli => "terraform-cli".to_string(),
            WarmTarget::Provider { namespace, name } => format!("provider:{namespace}/{name}"),
            WarmTarget::Module {
                namespace,
                name,
                provider,
            } => format!("module:{namespace}/{name}/{provider}"),
            WarmTarget::GitRepo { url } => format!("git:{url}"),
        }
    }

    /// Whether this target's on-disk cache already exists. Exposed so
    /// callers doing per-document (rather than whole-workspace)
    /// warming — `tfls-lsp`'s `version_prefetch` — can filter to
    /// uncached targets before deciding whether to even show
    /// progress UI, without duplicating the per-kind cache-path
    /// logic.
    pub fn is_cached(&self) -> bool {
        match self {
            WarmTarget::TerraformCli => tfls_provider_protocol::tool_versions::is_cached(),
            WarmTarget::Provider { namespace, name } => {
                tfls_provider_protocol::registry_versions::is_provider_cached(namespace, name)
            }
            WarmTarget::Module {
                namespace,
                name,
                provider,
            } => tfls_provider_protocol::registry_versions::is_module_cached(
                namespace, name, provider,
            ),
            WarmTarget::GitRepo { url } => {
                tfls_provider_protocol::git_refs::is_repo_tags_cached(url)
            }
        }
    }
}

/// Options gating how a target is warmed. Mirrors the LSP's
/// `cliEnabled` config flag: git tag-list resolution can shell out to
/// `git`, which some sandboxed CI runners disallow.
#[derive(Debug, Clone, Copy, Default)]
pub struct WarmOptions {
    pub cli_enabled: bool,
}

/// Outcome of a `warm_caches` call.
#[derive(Debug, Clone, Default)]
pub struct WarmReport {
    /// Targets that were fetched (attempted, regardless of success —
    /// see `failed` for which of these actually errored).
    pub fetched: usize,
    /// Targets skipped because the on-disk cache already covered them.
    pub cached: usize,
    /// `(target label, error message)` for every fetch that failed.
    /// Network failures here are expected in CI and must never abort
    /// the caller — see `tfls-lint`'s `--offline`-less default path.
    pub failed: Vec<(String, String)>,
}

/// Progress sink for a `warm_caches` run. `NoopProgress` is the default
/// for callers without a UI (e.g. `tfls-lint`); the LSP wraps its
/// `ProgressReporter` in an impl of this trait.
pub trait WarmProgress: Send + Sync {
    fn report(&self, done: usize, total: usize, message: &str);
}

/// No-op `WarmProgress` for callers that don't want progress
/// reporting (e.g. `tfls-lint`, which prints its own summary line
/// after the fact instead).
pub struct NoopProgress;

impl WarmProgress for NoopProgress {
    fn report(&self, _done: usize, _total: usize, _message: &str) {}
}

/// Walks every indexed document in `state` and collects the set of
/// cache-warmable targets referenced by `terraform { required_version
/// / required_providers }` and `module { source }` blocks — the same
/// extraction `tfls-lsp`'s per-document prefetch performs, just across
/// the whole workspace instead of one document.
pub fn collect_warm_targets(state: &StateStore) -> Vec<WarmTarget> {
    let mut out: HashSet<WarmTarget> = HashSet::new();
    for entry in state.documents.iter() {
        let Some(body) = entry.value().parsed.body.as_ref() else {
            continue;
        };
        collect_body_targets(body, &mut out);
    }
    out.into_iter().collect()
}

/// Collects the cache-warmable targets referenced by a single parsed
/// document body — the per-document counterpart to
/// [`collect_warm_targets`], used by `tfls-lsp`'s `version_prefetch`
/// on `did_open`/`did_change` for the one document that just changed,
/// rather than re-walking the whole workspace.
pub fn collect_targets_from_body(body: &Body) -> HashSet<WarmTarget> {
    let mut out = HashSet::new();
    collect_body_targets(body, &mut out);
    out
}

fn collect_body_targets(body: &Body, out: &mut HashSet<WarmTarget>) {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        match block.ident.as_str() {
            "terraform" => collect_terraform(&block.body, out),
            "module" => collect_module(&block.body, out),
            _ => {}
        }
    }
}

fn collect_terraform(body: &Body, out: &mut HashSet<WarmTarget>) {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if attr.key.as_str() == "required_version" && literal_string(&attr.value).is_some() {
                out.insert(WarmTarget::TerraformCli);
            }
        } else if let Some(nested) = structure.as_block() {
            if nested.ident.as_str() == "required_providers" {
                for entry in nested.body.iter() {
                    let Some(attr) = entry.as_attribute() else {
                        continue;
                    };
                    let Expression::Object(obj) = &attr.value else {
                        continue;
                    };
                    for (key, value) in obj.iter() {
                        if let Some(k) = object_key_as_str(key) {
                            if k == "source" {
                                if let Some(s) = literal_string(value.expr()) {
                                    if let Some((ns, name)) = parse_provider_source(&s) {
                                        out.insert(WarmTarget::Provider {
                                            namespace: ns,
                                            name,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn collect_module(body: &Body, out: &mut HashSet<WarmTarget>) {
    let mut source_str: Option<String> = None;
    for structure in body.iter() {
        let Some(attr) = structure.as_attribute() else {
            continue;
        };
        if attr.key.as_str() == "source" {
            source_str = literal_string(&attr.value);
        }
    }
    if let Some(s) = source_str.as_deref() {
        if let Some(reg) = parse_module_source(s) {
            out.insert(WarmTarget::Module {
                namespace: reg.0,
                name: reg.1,
                provider: reg.2,
            });
        } else if tfls_diag::is_git_source(s) {
            // Dedup by normalized repo URL so a monorepo's N subdir modules
            // trigger a single tag-list fetch.
            if let Some(url) = tfls_provider_protocol::git_refs::normalize_git_url(s) {
                out.insert(WarmTarget::GitRepo { url });
            }
        }
    }
}

fn literal_string(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(s.as_str().to_string()),
        Expression::StringTemplate(t) => {
            let mut collected = String::new();
            for element in t.iter() {
                match element {
                    hcl_edit::template::Element::Literal(lit) => collected.push_str(lit.as_str()),
                    _ => return None,
                }
            }
            Some(collected)
        }
        _ => None,
    }
}

fn object_key_as_str(key: &hcl_edit::expr::ObjectKey) -> Option<String> {
    match key {
        hcl_edit::expr::ObjectKey::Ident(d) => Some(d.as_str().to_string()),
        hcl_edit::expr::ObjectKey::Expression(Expression::String(s)) => {
            Some(s.as_str().to_string())
        }
        _ => None,
    }
}

fn parse_provider_source(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let mut parts = s.splitn(3, '/');
    let a = parts.next()?;
    let b = parts.next()?;
    if let Some(c) = parts.next() {
        Some((b.to_string(), c.to_string()))
    } else {
        Some((a.to_string(), b.to_string()))
    }
}

fn parse_module_source(s: &str) -> Option<(String, String, String)> {
    let s = s.trim();
    if s.starts_with('.') || s.starts_with('/') || s.contains("://") || s.contains("::") {
        return None;
    }
    let parts: Vec<&str> = s.split('/').collect();
    match parts.as_slice() {
        [ns, name, provider] if !ns.is_empty() && !name.is_empty() && !provider.is_empty() => {
            Some((ns.to_string(), name.to_string(), provider.to_string()))
        }
        _ => None,
    }
}

/// Fetches (or confirms already-cached) every target in `targets`,
/// reporting progress via `progress`. Runs all fetches concurrently
/// via `futures::future::join_all` — the engine has no tokio
/// dependency of its own, so the caller's runtime drives the futures;
/// this only requires an async executor be polling them (both
/// `tfls-lsp`'s `tokio::spawn` tasks and `tfls-lint`'s
/// `tokio::runtime::Runtime::block_on` satisfy that).
///
/// Never returns an `Err` — a fetch failure is recorded in
/// `WarmReport::failed` rather than aborting the batch, since a cold
/// or unreachable registry must not block linting/serving the rest of
/// the workspace.
pub async fn warm_caches(
    targets: &[WarmTarget],
    opts: &WarmOptions,
    progress: &dyn WarmProgress,
) -> WarmReport {
    let mut report = WarmReport::default();
    let total = targets.len();
    if total == 0 {
        return report;
    }

    let mut to_fetch = Vec::with_capacity(targets.len());
    for target in targets {
        if target.is_cached() {
            report.cached += 1;
        } else {
            to_fetch.push(target.clone());
        }
    }
    progress.report(report.cached, total, "checking cache");
    if to_fetch.is_empty() {
        return report;
    }

    // Build both HTTP clients once, up front. A build failure (e.g. TLS
    // backend init) means every fetch of that kind fails uniformly —
    // recorded per-target below rather than aborting the whole batch,
    // since git-ref targets don't need either client and should still
    // get a chance to warm.
    let http =
        tfls_provider_protocol::registry_versions::build_http_client().map_err(|e| e.to_string());
    let gh = tfls_provider_protocol::tool_versions::build_http_client().map_err(|e| e.to_string());
    let cli_enabled = opts.cli_enabled;

    let futs = to_fetch.into_iter().map(|target| {
        let http = http.clone();
        let gh = gh.clone();
        async move {
            let label = target.label();
            let result: Result<(), String> = match target {
                WarmTarget::TerraformCli => match gh {
                    Ok(gh) => tfls_provider_protocol::tool_versions::fetch_tool_versions(&gh)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e),
                },
                WarmTarget::Provider { namespace, name } => match http {
                    Ok(http) => tfls_provider_protocol::registry_versions::fetch_versions(
                        &http, &namespace, &name,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                    Err(e) => Err(e),
                },
                WarmTarget::Module {
                    namespace,
                    name,
                    provider,
                } => match http {
                    Ok(http) => tfls_provider_protocol::registry_versions::fetch_module_versions(
                        &http, &namespace, &name, &provider,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                    Err(e) => Err(e),
                },
                WarmTarget::GitRepo { url } => {
                    tfls_provider_protocol::git_refs::list_repo_tags(&url, cli_enabled)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            };
            (label, result)
        }
    });

    let results = futures::future::join_all(futs).await;
    let mut done = report.cached;
    for (label, result) in results {
        report.fetched += 1;
        done += 1;
        match result {
            Ok(()) => {}
            Err(e) => report.failed.push((label, e)),
        }
        progress.report(done, total, "fetching version catalog(s)");
    }

    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tfls_state::DocumentState;
    use url::Url;

    // Serializes tests in this module that mutate the process-wide
    // `XDG_CACHE_HOME` env var — `cargo test` runs tests in one binary
    // in parallel, and the other provider-protocol crates already use
    // the same pattern for the same reason.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn doc_with(text: &str) -> StateStore {
        let state = StateStore::new();
        let uri = Url::parse("file:///work/main.tf").unwrap();
        state.upsert_document(DocumentState::new(uri, text, 0));
        state
    }

    #[test]
    fn collect_warm_targets_finds_cli_provider_and_git_module() {
        let state = doc_with(
            r#"
terraform {
  required_version = ">= 1.5"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

module "m" {
  source = "git::https://example.com/x.git?ref=v1.2.3"
}
"#,
        );

        let targets = collect_warm_targets(&state);

        assert!(
            targets.contains(&WarmTarget::TerraformCli),
            "expected a TerraformCli target, got {targets:?}"
        );
        assert!(
            targets.iter().any(|t| matches!(
                t,
                WarmTarget::Provider { namespace, name }
                    if namespace == "hashicorp" && name == "aws"
            )),
            "expected a hashicorp/aws Provider target, got {targets:?}"
        );
        assert!(
            targets
                .iter()
                .any(|t| matches!(t, WarmTarget::GitRepo { .. })),
            "expected a GitRepo target, got {targets:?}"
        );
        assert_eq!(targets.len(), 3, "expected exactly 3 targets: {targets:?}");
    }

    #[test]
    fn collect_warm_targets_empty_for_doc_with_no_constraints() {
        let state = doc_with("resource \"aws_instance\" \"x\" {}\n");
        assert!(collect_warm_targets(&state).is_empty());
    }

    /// Pre-populates the on-disk caches `WarmTarget::is_cached` reads
    /// (mirroring `target_is_cached` in `tfls-lsp`'s
    /// `version_prefetch.rs`) under an isolated `XDG_CACHE_HOME`, then
    /// asserts `warm_caches` reports every target already cached and
    /// performs zero network fetches. Fetching is hard-wired to
    /// `reqwest` with no injectable HTTP boundary, so this is the
    /// cache-accounting test the task calls for in place of a mocked
    /// fetcher.
    #[tokio::test]
    async fn warm_caches_reports_cached_with_no_network() {
        let _env = ENV_LOCK.lock().await;
        let tmp = std::env::temp_dir().join(format!(
            "tfls-engine-prefetch-test-{}-{}",
            std::process::id(),
            "cached"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        // SAFETY: `ENV_LOCK` guarantees exclusive access to
        // `XDG_CACHE_HOME` while held.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", &tmp);
        }

        // Terraform CLI cache: both terraform.json and opentofu.json
        // must exist (see `tool_versions::is_cached`).
        let tool_dir = tmp.join("terraform-ls-rs").join("tool-versions");
        std::fs::create_dir_all(&tool_dir).unwrap();
        std::fs::write(tool_dir.join("terraform.json"), "[]").unwrap();
        std::fs::write(tool_dir.join("opentofu.json"), "[]").unwrap();

        // Provider cache: at least one of terraform/opentofu registry
        // dirs must exist (see `registry_versions::is_provider_cached`).
        let provider_dir = tmp
            .join("terraform-ls-rs")
            .join("registry-versions")
            .join("terraform")
            .join("hashicorp")
            .join("aws");
        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::write(provider_dir.join("versions.json"), "[]").unwrap();

        let targets = vec![
            WarmTarget::TerraformCli,
            WarmTarget::Provider {
                namespace: "hashicorp".to_string(),
                name: "aws".to_string(),
            },
        ];

        let report =
            warm_caches(&targets, &WarmOptions { cli_enabled: false }, &NoopProgress).await;

        assert_eq!(report.cached, targets.len());
        assert_eq!(report.fetched, 0);
        assert!(report.failed.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn warm_caches_empty_targets_is_a_noop() {
        let report = warm_caches(&[], &WarmOptions::default(), &NoopProgress).await;
        assert_eq!(report.cached, 0);
        assert_eq!(report.fetched, 0);
        assert!(report.failed.is_empty());
    }
}
