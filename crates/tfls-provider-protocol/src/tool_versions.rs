//! Fetch Terraform and OpenTofu CLI release versions from their
//! GitHub `/releases` feeds, so completion can suggest real values
//! inside `required_version = "…"` under a top-level `terraform {}`
//! block.
//!
//! Behaves the same way as `registry_versions`: parallel fetch of
//! both feeds, merged + tagged with provenance (`terraform only`,
//! `opentofu only`, or `terraform + opentofu` for the versions both
//! projects happened to release), 24h disk cache with stale-cache
//! fallback when a GitHub outage / rate-limit kicks in.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::ProtocolError;
use crate::registry_versions::{VersionInfo, merge_with_provenance};

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const USER_AGENT: &str = "terraform-ls-rs/0.1 (+tool-versions)";

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Fetch merged CLI versions from both Terraform and OpenTofu
/// GitHub release feeds. See the crate-level doc comment for
/// outage / caching semantics — they match `registry_versions`.
pub async fn fetch_tool_versions(
    client: &reqwest::Client,
) -> Result<Vec<VersionInfo>, ProtocolError> {
    let tf = fetch_github_releases(client, "hashicorp", "terraform", "terraform");
    let tofu = fetch_github_releases(client, "opentofu", "opentofu", "opentofu");
    let (tf_res, tofu_res) = tokio::join!(tf, tofu);
    let tf_vec = tf_res.unwrap_or_default();
    let tofu_vec = tofu_res.unwrap_or_default();
    Ok(merge_with_provenance(tf_vec, tofu_vec))
}

/// HTTP client appropriate for the GitHub REST API: user-agent set
/// (GitHub requires one), 20-second timeout, rustls-webpki roots.
pub fn build_http_client() -> Result<reqwest::Client, ProtocolError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))
}

async fn fetch_github_releases(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    cache_slug: &str,
) -> Result<Vec<String>, ProtocolError> {
    let cache_path = tool_cache_path(cache_slug);
    if let Some(fresh) = read_fresh_cache(&cache_path).await {
        return Ok(fresh);
    }
    match try_github_fetch(client, owner, repo).await {
        Ok(versions) => {
            write_cache(&cache_path, &versions).await;
            Ok(versions)
        }
        Err(e) => {
            if let Some(stale) = read_any_cache(&cache_path).await {
                tracing::debug!(
                    error = %e,
                    %owner, %repo,
                    "github release fetch failed; serving stale cache"
                );
                Ok(stale)
            } else {
                Err(e)
            }
        }
    }
}

async fn try_github_fetch(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
) -> Result<Vec<String>, ProtocolError> {
    // 100 is GitHub's max page size; Terraform and OpenTofu combined
    // have well under 100 stable releases so one page suffices.
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases?per_page=100");
    tracing::debug!(%url, "fetching github releases");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
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
    let releases: Vec<GitHubRelease> = serde_json::from_str(&body)
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    Ok(releases
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease)
        .map(|r| r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name).to_string())
        .collect())
}

fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("terraform-ls-rs").join("tool-versions");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("terraform-ls-rs")
            .join("tool-versions");
    }
    PathBuf::from("/tmp/terraform-ls-rs/tool-versions")
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

fn tool_cache_path(slug: &str) -> PathBuf {
    cache_root().join(format!("{}.json", sanitise(slug)))
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
    fn tool_cache_path_is_slug_scoped() {
        let tf = tool_cache_path("terraform");
        let tofu = tool_cache_path("opentofu");
        assert_ne!(tf, tofu);
        assert!(tf.ends_with("terraform.json"));
        assert!(tofu.ends_with("opentofu.json"));
    }

    #[test]
    fn sanitise_normalises_slashes() {
        assert_eq!(sanitise("opentofu"), "opentofu");
        assert_eq!(sanitise("weird/slug"), "weird_slug");
    }

    // The outage / stale-cache behaviour is shared with
    // `registry_versions` through identical helper patterns and is
    // exercised by that module's `stale_cache_serves_during_outage`
    // test. Duplicating it here would either flake against the real
    // GitHub API or race on the process-wide `XDG_CACHE_HOME` env var
    // with the sibling test — skip it and rely on the shared contract.
}
