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
    if let Some(cached) = read_fresh_cache(&cache_path).await {
        return Ok(cached);
    }
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
    let versions: Vec<String> = parsed.versions.into_iter().map(|v| v.version).collect();
    // Write the parsed + filtered form, not the raw response — keeps
    // the cache portable across registry response shape changes.
    write_cache(&cache_path, &versions).await;
    Ok(versions)
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
    fn sanitise_rejects_path_traversal() {
        assert_eq!(sanitise("../etc/passwd"), "___etc_passwd");
        assert_eq!(sanitise("hashicorp"), "hashicorp");
        assert_eq!(sanitise("aws-plus"), "aws-plus");
    }

    #[test]
    fn cache_path_is_registry_scoped() {
        let p1 = versions_cache_path("terraform", "hashicorp", "aws");
        let p2 = versions_cache_path("opentofu", "hashicorp", "aws");
        assert_ne!(p1, p2, "same provider must cache under different registries");
    }
}
