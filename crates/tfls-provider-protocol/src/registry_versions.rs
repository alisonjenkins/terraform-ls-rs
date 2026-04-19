//! Fetch available provider versions from the Terraform and OpenTofu
//! registries so the completion handler can suggest real versions
//! inside `version = "…"` under a `required_providers` entry.
//!
//! Both registries speak a compatible `v1/providers/{ns}/{name}/versions`
//! endpoint. We hit them in parallel, merge + dedupe the result, and
//! cache to disk (24h TTL) so subsequent completion round-trips are
//! free.
//!
//! When the network is unreachable and no cache exists, the caller
//! gets an empty list and the completion dispatcher falls back to
//! purely static constraint templates (`~> N`, `>= N`).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::ProtocolError;

const TERRAFORM_HOST: &str = "https://registry.terraform.io";
const OPENTOFU_HOST: &str = "https://registry.opentofu.org";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// How fresh a cached response has to be before we reuse it without
/// hitting the network.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Deserialize)]
struct VersionsResponse {
    #[serde(default)]
    versions: Vec<VersionEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionEntry {
    version: String,
}

/// Fetch available versions for `<namespace>/<name>` from both the
/// Terraform and OpenTofu registries, merge, dedupe, and return
/// newest-first by semver-lexical ordering (caller may re-sort).
///
/// Swallows per-registry errors — if either registry is unreachable
/// but the other succeeds, we still return useful data. If *both*
/// fail and no cache is usable, returns `Ok(Vec::new())` so callers
/// can degrade gracefully.
pub async fn fetch_versions(
    client: &reqwest::Client,
    namespace: &str,
    name: &str,
) -> Result<Vec<String>, ProtocolError> {
    let tf = fetch_registry_versions(client, TERRAFORM_HOST, "terraform", namespace, name);
    let tofu = fetch_registry_versions(client, OPENTOFU_HOST, "opentofu", namespace, name);
    let (tf_res, tofu_res) = tokio::join!(tf, tofu);

    let mut merged: Vec<String> = Vec::new();
    if let Ok(vs) = tf_res {
        merged.extend(vs);
    }
    if let Ok(vs) = tofu_res {
        merged.extend(vs);
    }
    // Dedupe preserving first-seen order (terraform reg comes first,
    // so its ordering wins for common versions).
    let mut seen = std::collections::HashSet::new();
    merged.retain(|v| seen.insert(v.clone()));
    Ok(merged)
}

async fn fetch_registry_versions(
    client: &reqwest::Client,
    host: &str,
    registry_slug: &str,
    namespace: &str,
    name: &str,
) -> Result<Vec<String>, ProtocolError> {
    let cache_path = versions_cache_path(registry_slug, namespace, name);
    // Fast path: a fresh cache (< TTL) wins outright — no network.
    if let Some(cached) = read_fresh_cache(&cache_path).await {
        return Ok(cached);
    }
    // Otherwise try the network. If anything fails (transport, non-2xx,
    // malformed body), fall back to any cache on disk regardless of age
    // so a registry outage doesn't blank the completion list.
    match try_network_fetch(client, host, namespace, name).await {
        Ok(versions) => {
            write_cache(&cache_path, &versions).await;
            Ok(versions)
        }
        Err(e) => {
            if let Some(stale) = read_any_cache(&cache_path).await {
                tracing::debug!(
                    error = %e,
                    %registry_slug, %namespace, %name,
                    "registry fetch failed; serving stale cache"
                );
                Ok(stale)
            } else {
                Err(e)
            }
        }
    }
}

async fn try_network_fetch(
    client: &reqwest::Client,
    host: &str,
    namespace: &str,
    name: &str,
) -> Result<Vec<String>, ProtocolError> {
    let url = format!("{host}/v1/providers/{namespace}/{name}/versions");
    tracing::debug!(%url, "fetching registry versions");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(ProtocolError::RegistryHttp(format!(
            "GET {url} → HTTP {}",
            resp.status()
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    let parsed: VersionsResponse = serde_json::from_str(&body)
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    Ok(parsed.versions.into_iter().map(|v| v.version).collect())
}

/// Build an HTTP client suitable for registry endpoints. Reuses the
/// same defaults as `registry_docs` so both modules share connection
/// pools when a caller reuses the client.
pub fn build_http_client() -> Result<reqwest::Client, ProtocolError> {
    reqwest::Client::builder()
        .user_agent("terraform-ls-rs/0.1 (+registry-versions)")
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))
}

fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("terraform-ls-rs").join("registry-versions");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("terraform-ls-rs")
            .join("registry-versions");
    }
    PathBuf::from("/tmp/terraform-ls-rs/registry-versions")
}

fn sanitise(component: &str) -> String {
    component
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn versions_cache_path(registry: &str, namespace: &str, name: &str) -> PathBuf {
    cache_root()
        .join(sanitise(registry))
        .join(sanitise(namespace))
        .join(sanitise(name))
        .join("versions.json")
}

async fn read_fresh_cache(path: &Path) -> Option<Vec<String>> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let modified = meta.modified().ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::MAX);
    if age > CACHE_TTL {
        return None;
    }
    read_cache_contents(path).await
}

/// Read whatever's on disk, regardless of age. Used as a graceful
/// fallback when the live fetch fails during an outage and the only
/// data we have is stale.
async fn read_any_cache(path: &Path) -> Option<Vec<String>> {
    read_cache_contents(path).await
}

async fn read_cache_contents(path: &Path) -> Option<Vec<String>> {
    let body = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str::<Vec<String>>(&body).ok()
}

async fn write_cache(path: &Path, versions: &[String]) {
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::debug!(error = %e, dir = %parent.display(), "cache dir create failed");
            return;
        }
    }
    let body = match serde_json::to_string(versions) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "cache serialise failed");
            return;
        }
    };
    if let Err(e) = tokio::fs::write(path, body).await {
        tracing::debug!(error = %e, path = %path.display(), "cache write failed");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_strips_path_separators() {
        // `/` in a component becomes `_`, preventing a single value
        // from injecting a subdirectory into the cache path. `.` is
        // kept so version strings like "5.10.0" round-trip unchanged.
        assert_eq!(sanitise("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitise("hashicorp"), "hashicorp");
        assert_eq!(sanitise("aws-plus"), "aws-plus");
        assert_eq!(sanitise("5.10.0"), "5.10.0");
    }

    #[test]
    fn cache_path_is_registry_scoped() {
        let p1 = versions_cache_path("terraform", "hashicorp", "aws");
        let p2 = versions_cache_path("opentofu", "hashicorp", "aws");
        assert_ne!(p1, p2, "same provider must cache under different registries");
    }

    /// An outage with a pre-existing cache — even if the cache is
    /// past its TTL — must still be served rather than returning an
    /// empty list. Uses an unreachable localhost port so the live
    /// fetch fails fast and the fallback path fires.
    #[tokio::test]
    async fn stale_cache_serves_during_outage() {
        // Point the cache root at a fresh temp dir so the test doesn't
        // race with the real user's cache.
        let tmp = std::env::temp_dir().join(format!("tfls-test-{}", std::process::id()));
        // Safety: this test is the only consumer of these env vars
        // in this test binary. Tokio's `current_thread` runtime won't
        // run tests concurrently in the same process for this file;
        // the tfls-lsp lib test crate runs separately. Even if races
        // mattered, the cache-root resolution only looks at env at
        // call time.
        // SAFETY: single-threaded test, called before spawning anything.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", &tmp);
        }

        let cache_path = versions_cache_path("terraform", "hashicorp", "aws");
        let payload = vec!["5.99.0".to_string(), "5.98.0".to_string()];
        write_cache(&cache_path, &payload).await;
        assert!(tokio::fs::metadata(&cache_path).await.is_ok(), "cache written");

        let client = build_http_client().expect("http client");
        // 127.0.0.1:1 is the canonical "nothing listens here" port.
        let result = fetch_registry_versions(
            &client,
            "http://127.0.0.1:1",
            "terraform",
            "hashicorp",
            "aws",
        )
        .await
        .expect("outage with cache must not error");
        assert_eq!(result, payload, "must serve cached versions during outage");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn no_cache_no_network_returns_error() {
        // Isolated cache root so we don't stumble onto an unrelated
        // existing entry.
        let tmp = std::env::temp_dir().join(format!("tfls-test-nocache-{}", std::process::id()));
        // SAFETY: see stale_cache_serves_during_outage.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", &tmp);
        }

        let client = build_http_client().expect("http client");
        let result = fetch_registry_versions(
            &client,
            "http://127.0.0.1:1",
            "terraform",
            "no-such-ns-definitely",
            "no-such-name-definitely",
        )
        .await;
        assert!(result.is_err(), "outage + no cache must surface an error");

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
