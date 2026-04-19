//! Fetch the full public provider catalog from the Terraform registry
//! so source-line completion can offer more than the hand-curated list
//! of popular providers baked into `tfls-core::builtin_blocks`.
//!
//! Scope: **official** and **partner** tier providers only. Community
//! tier is thousands of rarely-used providers — shipping them as
//! completion items would dominate the list with noise. Users who
//! need a community provider still just type `ns/name` directly.
//!
//! Fetched once per 7 days to disk
//! (`$XDG_CACHE_HOME/terraform-ls-rs/registry-catalog/catalog.json`)
//! and served from cache on subsequent completions. Stale-cache
//! fallback during registry outages matches `registry_versions`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::ProtocolError;
use crate::registry_versions::Registry;

const TERRAFORM_HOST: &str = "https://registry.terraform.io";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Catalog changes slowly — weekly refresh is plenty.
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Cap on pages per tier. 20 pages × 100 = 2000 entries is enough to
/// cover every official + partner provider with room to spare.
const MAX_PAGES_PER_TIER: u32 = 20;
const USER_AGENT: &str = "terraform-ls-rs/0.1 (+registry-catalog)";

/// One provider entry in the public catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub namespace: String,
    pub name: String,
    /// "official", "partner", or "community". `None` if the registry
    /// didn't tell us.
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Which registries advertise this provider. Phase-1 only fetches
    /// from the Terraform registry, so every entry today carries
    /// `[Terraform]`; the shape is future-proofed for when the
    /// OpenTofu catalog is added.
    #[serde(default)]
    pub registries: Vec<Registry>,
}

impl CatalogEntry {
    /// `"namespace/name"` — the source value the user would type.
    pub fn source(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

/// Fetch the public provider catalog (official + partner tiers) from
/// the Terraform registry. Transparently serves from disk cache
/// (fresh or, during an outage, stale).
pub async fn fetch_catalog(
    client: &reqwest::Client,
) -> Result<Vec<CatalogEntry>, ProtocolError> {
    let cache_path = catalog_cache_path();
    if let Some(fresh) = read_fresh_cache(&cache_path).await {
        return Ok(fresh);
    }
    match try_live_fetch(client).await {
        Ok(entries) => {
            write_cache(&cache_path, &entries).await;
            Ok(entries)
        }
        Err(e) => {
            if let Some(stale) = read_any_cache(&cache_path).await {
                tracing::debug!(error = %e, "catalog fetch failed; serving stale cache");
                Ok(stale)
            } else {
                Err(e)
            }
        }
    }
}

/// HTTP client appropriate for the Terraform registry v2 API. 30-second
/// timeout is generous to give multi-page fetches room to complete.
pub fn build_http_client() -> Result<reqwest::Client, ProtocolError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))
}

async fn try_live_fetch(
    client: &reqwest::Client,
) -> Result<Vec<CatalogEntry>, ProtocolError> {
    // Fetch official and partner tiers in parallel — they're independent.
    let (official_res, partner_res) = tokio::join!(
        fetch_tier(client, "official"),
        fetch_tier(client, "partner"),
    );
    let mut entries = official_res.unwrap_or_default();
    entries.extend(partner_res.unwrap_or_default());
    // If both failed with no data at all, surface the error so the
    // caller falls back to the stale cache.
    if entries.is_empty() {
        return Err(ProtocolError::RegistryHttp(
            "catalog fetch returned no entries".to_string(),
        ));
    }
    // Dedupe by (namespace, name) in case a provider was listed under
    // both tiers somehow.
    entries.sort_by(|a, b| {
        (a.namespace.as_str(), a.name.as_str()).cmp(&(b.namespace.as_str(), b.name.as_str()))
    });
    entries.dedup_by(|a, b| a.namespace == b.namespace && a.name == b.name);
    Ok(entries)
}

async fn fetch_tier(
    client: &reqwest::Client,
    tier: &str,
) -> Result<Vec<CatalogEntry>, ProtocolError> {
    let mut out: Vec<CatalogEntry> = Vec::new();
    for page in 1..=MAX_PAGES_PER_TIER {
        let url = format!(
            "{TERRAFORM_HOST}/v2/providers?filter%5Btier%5D={tier}&page%5Bsize%5D=100&page%5Bnumber%5D={page}"
        );
        tracing::debug!(%url, "fetching registry catalog page");
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
        let parsed: CatalogResponse = serde_json::from_str(&body)
            .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
        let page_len = parsed.data.len();
        for item in parsed.data {
            let attrs = item.attributes;
            out.push(CatalogEntry {
                namespace: attrs.namespace,
                name: attrs.name,
                tier: attrs.tier,
                description: attrs.description,
                registries: vec![Registry::Terraform],
            });
        }
        // Stop when the last page returned < 100 items, OR when the
        // pagination metadata says we've exhausted pages.
        if page_len < 100 {
            break;
        }
        if let Some(meta) = parsed.meta {
            if let Some(pagination) = meta.pagination {
                if page >= pagination.total_pages {
                    break;
                }
            }
        }
    }
    Ok(out)
}

// --- Response deserialisation ---------------------------------------------

#[derive(Debug, Deserialize)]
struct CatalogResponse {
    #[serde(default)]
    data: Vec<CatalogProvider>,
    #[serde(default)]
    meta: Option<CatalogMeta>,
}

#[derive(Debug, Deserialize)]
struct CatalogProvider {
    attributes: CatalogAttributes,
}

#[derive(Debug, Deserialize)]
struct CatalogAttributes {
    namespace: String,
    name: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CatalogMeta {
    #[serde(default)]
    pagination: Option<CatalogPagination>,
}

#[derive(Debug, Deserialize)]
struct CatalogPagination {
    #[serde(rename = "total-pages", default)]
    total_pages: u32,
}

// --- Cache helpers --------------------------------------------------------

fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir)
            .join("terraform-ls-rs")
            .join("registry-catalog");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("terraform-ls-rs")
            .join("registry-catalog");
    }
    PathBuf::from("/tmp/terraform-ls-rs/registry-catalog")
}

fn catalog_cache_path() -> PathBuf {
    cache_root().join("catalog.json")
}

async fn read_fresh_cache(path: &Path) -> Option<Vec<CatalogEntry>> {
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

async fn read_any_cache(path: &Path) -> Option<Vec<CatalogEntry>> {
    read_cache_contents(path).await
}

async fn read_cache_contents(path: &Path) -> Option<Vec<CatalogEntry>> {
    let body = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str::<Vec<CatalogEntry>>(&body).ok()
}

async fn write_cache(path: &Path, entries: &[CatalogEntry]) {
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::debug!(error = %e, dir = %parent.display(), "cache dir create failed");
            return;
        }
    }
    let body = match serde_json::to_string(entries) {
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
    fn parses_catalog_response_shape() {
        let body = r#"{
            "data": [
                {
                    "attributes": {
                        "namespace": "hashicorp",
                        "name": "aws",
                        "tier": "official",
                        "description": "AWS"
                    }
                },
                {
                    "attributes": {
                        "namespace": "cloudflare",
                        "name": "cloudflare",
                        "tier": "partner"
                    }
                }
            ],
            "meta": {
                "pagination": {"total-pages": 1}
            }
        }"#;
        let parsed: CatalogResponse = serde_json::from_str(body).expect("parse");
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[0].attributes.namespace, "hashicorp");
        assert_eq!(parsed.data[0].attributes.name, "aws");
        assert_eq!(parsed.data[0].attributes.tier.as_deref(), Some("official"));
        assert_eq!(parsed.meta.unwrap().pagination.unwrap().total_pages, 1);
    }

    #[test]
    fn catalog_entry_source_combines_namespace_and_name() {
        let e = CatalogEntry {
            namespace: "hashicorp".to_string(),
            name: "aws".to_string(),
            tier: None,
            description: None,
            registries: vec![Registry::Terraform],
        };
        assert_eq!(e.source(), "hashicorp/aws");
    }

    #[test]
    fn cache_path_is_under_registry_catalog_dir() {
        let p = catalog_cache_path();
        let s = p.to_string_lossy();
        assert!(s.contains("registry-catalog"), "got {s}");
        assert!(s.ends_with("catalog.json"));
    }
}
