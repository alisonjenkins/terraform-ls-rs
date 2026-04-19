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

use serde::{Deserialize, Serialize};

use crate::ProtocolError;

const TERRAFORM_HOST: &str = "https://registry.terraform.io";
const OPENTOFU_HOST: &str = "https://registry.opentofu.org";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// How fresh a cached response has to be before we reuse it without
/// hitting the network.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Which public registry a version was advertised by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Registry {
    Terraform,
    OpenTofu,
}

impl Registry {
    pub fn label(self) -> &'static str {
        match self {
            Registry::Terraform => "terraform",
            Registry::OpenTofu => "opentofu",
        }
    }
}

/// One version as reported by the registries, tagged with which
/// registry/registries it was found in so the UI can surface
/// provenance (most versions show up on both).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub registries: Vec<Registry>,
    /// ISO-8601 publication timestamp if we could obtain one (some
    /// endpoints return version strings only). Used for age-based
    /// inlay hints; `None` disables the age signal for this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}

impl VersionInfo {
    /// Short phrase for completion-item detail fields.
    /// "terraform + opentofu", "terraform only", "opentofu only",
    /// or "(registry unknown)" as a degenerate fallback.
    pub fn provenance_label(&self) -> String {
        let tf = self.registries.contains(&Registry::Terraform);
        let tofu = self.registries.contains(&Registry::OpenTofu);
        match (tf, tofu) {
            (true, true) => "terraform + opentofu".to_string(),
            (true, false) => "terraform only".to_string(),
            (false, true) => "opentofu only".to_string(),
            (false, false) => "(registry unknown)".to_string(),
        }
    }
}

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
/// Terraform and OpenTofu registries, merge, dedupe, and tag each
/// result with the registry/registries that advertised it. Ordering
/// preserves the Terraform registry's response order (newest-first
/// for both registries in practice) with any OpenTofu-only versions
/// appended at the end.
///
/// Swallows per-registry errors — if either registry is unreachable
/// but the other succeeds, we still return useful data. If *both*
/// fail and no cache is usable, returns `Ok(Vec::new())` so callers
/// can degrade to static constraint templates.
pub async fn fetch_versions(
    client: &reqwest::Client,
    namespace: &str,
    name: &str,
) -> Result<Vec<VersionInfo>, ProtocolError> {
    let tf = fetch_registry_versions(client, TERRAFORM_HOST, "terraform", namespace, name);
    let tofu = fetch_registry_versions(client, OPENTOFU_HOST, "opentofu", namespace, name);
    // Date enrichment runs in parallel with the primary fetches. It
    // hits the Terraform-registry v2 API (the only one that gives us
    // `published-at` in bulk). If it fails, we simply skip age-based
    // inlay hints for this provider — nothing else breaks.
    let dates = fetch_provider_dates(client, namespace, name);
    let (tf_res, tofu_res, dates_res) = tokio::join!(tf, tofu, dates);

    let tf_vec: Vec<String> = tf_res.unwrap_or_default();
    let tofu_vec: Vec<String> = tofu_res.unwrap_or_default();
    let mut merged = merge_with_provenance(tf_vec, tofu_vec);
    if let Ok(dates_map) = dates_res {
        attach_dates(&mut merged, &dates_map);
    }
    Ok(merged)
}

/// Resolve the Terraform-registry internal provider ID, then fetch
/// every version's `published-at`. Cached under
/// `registry-versions/dates/{ns}/{name}.json` separately from the
/// v1 version list so the two can evolve independently.
async fn fetch_provider_dates(
    client: &reqwest::Client,
    namespace: &str,
    name: &str,
) -> Result<std::collections::HashMap<String, String>, ProtocolError> {
    let cache_path = provider_dates_cache_path(namespace, name);
    if let Some(fresh) = read_fresh_dates_cache(&cache_path).await {
        return Ok(fresh);
    }
    match try_fetch_provider_dates(client, namespace, name).await {
        Ok(dates) => {
            write_dates_cache(&cache_path, &dates).await;
            Ok(dates)
        }
        Err(e) => {
            if let Some(stale) = read_any_dates_cache(&cache_path).await {
                tracing::debug!(error = %e, %namespace, %name,
                    "provider dates fetch failed; serving stale cache");
                Ok(stale)
            } else {
                Err(e)
            }
        }
    }
}

async fn try_fetch_provider_dates(
    client: &reqwest::Client,
    namespace: &str,
    name: &str,
) -> Result<std::collections::HashMap<String, String>, ProtocolError> {
    // Step 1 — look up the internal numeric provider ID.
    let lookup_url = format!(
        "{TERRAFORM_HOST}/v2/providers?filter%5Bnamespace%5D={namespace}&filter%5Bname%5D={name}&page%5Bsize%5D=1"
    );
    #[derive(Deserialize)]
    struct LookupResp {
        data: Vec<LookupEntry>,
    }
    #[derive(Deserialize)]
    struct LookupEntry {
        id: String,
    }
    let resp = client
        .get(&lookup_url)
        .send()
        .await
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(ProtocolError::RegistryHttp(format!(
            "GET {lookup_url} → HTTP {}",
            resp.status()
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    let lookup: LookupResp =
        serde_json::from_str(&body).map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    let Some(id) = lookup.data.first().map(|e| e.id.clone()) else {
        return Ok(std::collections::HashMap::new());
    };

    // Step 2 — paginate /v2/providers/{id}/provider-versions until we
    // run out. Providers with many releases (e.g. hashicorp/aws) get
    // ~20 pages of 100; small ones get one page.
    #[derive(Deserialize)]
    struct VersionsResp {
        data: Vec<VersionEntry>,
        #[serde(default)]
        meta: Option<MetaBlock>,
    }
    #[derive(Deserialize)]
    struct VersionEntry {
        attributes: VersionAttrs,
    }
    #[derive(Deserialize)]
    struct VersionAttrs {
        version: String,
        #[serde(rename = "published-at", default)]
        published_at: Option<String>,
    }
    #[derive(Deserialize)]
    struct MetaBlock {
        #[serde(default)]
        pagination: Option<PaginationBlock>,
    }
    #[derive(Deserialize)]
    struct PaginationBlock {
        #[serde(rename = "total-pages", default)]
        total_pages: u32,
    }
    let mut out: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for page in 1..=25u32 {
        let url = format!(
            "{TERRAFORM_HOST}/v2/providers/{id}/provider-versions?page%5Bsize%5D=100&page%5Bnumber%5D={page}"
        );
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
        let parsed: VersionsResp = serde_json::from_str(&body)
            .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
        let page_len = parsed.data.len();
        for entry in parsed.data {
            if let Some(date) = entry.attributes.published_at {
                out.insert(entry.attributes.version, date);
            }
        }
        if page_len < 100 {
            break;
        }
        if let Some(meta) = parsed.meta {
            if let Some(pg) = meta.pagination {
                if page >= pg.total_pages {
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn provider_dates_cache_path(namespace: &str, name: &str) -> PathBuf {
    cache_root()
        .join("dates")
        .join(sanitise(namespace))
        .join(sanitise(name))
        .join("dates.json")
}

async fn read_fresh_dates_cache(path: &Path) -> Option<std::collections::HashMap<String, String>> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let modified = meta.modified().ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::MAX);
    if age > CACHE_TTL {
        return None;
    }
    read_dates_cache(path).await
}

async fn read_any_dates_cache(path: &Path) -> Option<std::collections::HashMap<String, String>> {
    read_dates_cache(path).await
}

async fn read_dates_cache(path: &Path) -> Option<std::collections::HashMap<String, String>> {
    let body = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str::<std::collections::HashMap<String, String>>(&body).ok()
}

async fn write_dates_cache(path: &Path, dates: &std::collections::HashMap<String, String>) {
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::debug!(error = %e, dir = %parent.display(), "dates cache dir create failed");
            return;
        }
    }
    let body = match serde_json::to_string(dates) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "dates cache serialise failed");
            return;
        }
    };
    if let Err(e) = tokio::fs::write(path, body).await {
        tracing::debug!(error = %e, path = %path.display(), "dates cache write failed");
    }
}

/// Merge two version lists into one tagged with provenance, then
/// sort newest-first by semver. Sort is descending on
/// `(major, minor, patch, stability, pre-release identifier)` so
/// — stable `1.0.0` outranks `1.0.0-rc1` (matching the semver spec);
/// — non-parseable tag names fall to the bottom deterministically.
///
/// Pure function, no network — trivially unit-testable.
pub fn merge_with_provenance(tf: Vec<String>, tofu: Vec<String>) -> Vec<VersionInfo> {
    let tofu_set: std::collections::HashSet<String> = tofu.iter().cloned().collect();
    let mut out: Vec<VersionInfo> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for v in tf {
        if seen.insert(v.clone()) {
            let mut regs = vec![Registry::Terraform];
            if tofu_set.contains(&v) {
                regs.push(Registry::OpenTofu);
            }
            out.push(VersionInfo {
                version: v,
                registries: regs,
                published_at: None,
            });
        }
    }
    for v in tofu {
        if seen.insert(v.clone()) {
            out.push(VersionInfo {
                version: v,
                registries: vec![Registry::OpenTofu],
                published_at: None,
            });
        }
    }
    out.sort_by(|a, b| semver_key(&b.version).cmp(&semver_key(&a.version)));
    out
}

/// Stitch publication dates (keyed by version string) onto an already-
/// merged `VersionInfo` list. Missing keys are left as `None`. Used
/// by callers that enrich v1 version strings with v2 `published_at`
/// data after the primary fetch completes.
pub fn attach_dates(versions: &mut [VersionInfo], dates: &std::collections::HashMap<String, String>) {
    for v in versions.iter_mut() {
        if v.published_at.is_none() {
            if let Some(d) = dates.get(&v.version) {
                v.published_at = Some(d.clone());
            }
        }
    }
}

/// A comparable key for a semver-ish version string. Tuple shape lets
/// Rust's derived `Ord` do the right thing:
/// * parseable `major.minor.patch` sorts numerically;
/// * stable releases outrank pre-releases of the same core version
///   (`stability = 1` vs `0`);
/// * pre-release identifiers compare lexicographically (good enough
///   for the `alpha < beta < rc` / `alpha.1 < alpha.2` cases);
/// * unparseable tag names (`main`, `nightly-2026-04-19`) fall to
///   the very bottom via `major = i64::MIN`.
fn semver_key(v: &str) -> (i64, i64, i64, i32, String) {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    // Strip semver build metadata (`+sha.abcd`) — it doesn't affect
    // ordering per semver §11.
    let core = core.split('+').next().unwrap_or(core);
    let mut parts = core.splitn(3, '.');
    let major: Option<i64> = parts.next().and_then(|s| s.parse().ok());
    let minor: Option<i64> = parts.next().and_then(|s| s.parse().ok());
    let patch: Option<i64> = parts.next().and_then(|s| s.parse().ok());
    match major {
        Some(ma) => {
            let mi = minor.unwrap_or(0);
            let pa = patch.unwrap_or(0);
            let stability = if pre.is_some() { 0 } else { 1 };
            let pre_id = pre.unwrap_or("").to_string();
            (ma, mi, pa, stability, pre_id)
        }
        None => (i64::MIN, 0, 0, 0, v.to_string()),
    }
}

/// Fetch merged module versions for `<namespace>/<name>/<provider>`
/// from both the Terraform and OpenTofu registries. Same semantics
/// as `fetch_versions` for providers — parallel join, provenance
/// tagging, 24h disk cache, stale-cache outage fallback.
pub async fn fetch_module_versions(
    client: &reqwest::Client,
    namespace: &str,
    name: &str,
    provider: &str,
) -> Result<Vec<VersionInfo>, ProtocolError> {
    let tf = fetch_module_registry_versions(
        client, TERRAFORM_HOST, "terraform", namespace, name, provider,
    );
    let tofu = fetch_module_registry_versions(
        client, OPENTOFU_HOST, "opentofu", namespace, name, provider,
    );
    let (tf_res, tofu_res) = tokio::join!(tf, tofu);
    Ok(merge_with_provenance(
        tf_res.unwrap_or_default(),
        tofu_res.unwrap_or_default(),
    ))
}

async fn fetch_module_registry_versions(
    client: &reqwest::Client,
    host: &str,
    registry_slug: &str,
    namespace: &str,
    name: &str,
    provider: &str,
) -> Result<Vec<String>, ProtocolError> {
    let cache_path = module_versions_cache_path(registry_slug, namespace, name, provider);
    if let Some(cached) = read_fresh_cache(&cache_path).await {
        return Ok(cached);
    }
    match try_module_network_fetch(client, host, namespace, name, provider).await {
        Ok(versions) => {
            write_cache(&cache_path, &versions).await;
            Ok(versions)
        }
        Err(e) => {
            if let Some(stale) = read_any_cache(&cache_path).await {
                tracing::debug!(
                    error = %e, %registry_slug, %namespace, %name, %provider,
                    "module registry fetch failed; serving stale cache"
                );
                Ok(stale)
            } else {
                Err(e)
            }
        }
    }
}

async fn try_module_network_fetch(
    client: &reqwest::Client,
    host: &str,
    namespace: &str,
    name: &str,
    provider: &str,
) -> Result<Vec<String>, ProtocolError> {
    let url = format!("{host}/v1/modules/{namespace}/{name}/{provider}/versions");
    tracing::debug!(%url, "fetching module registry versions");
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
    // Module registries return a different envelope than provider
    // registries: `{ modules: [{ versions: [{ version: "…" }, …] }] }`.
    // We flatten to the version strings.
    #[derive(serde::Deserialize)]
    struct ModuleVersionsResp {
        #[serde(default)]
        modules: Vec<ModuleEntry>,
    }
    #[derive(serde::Deserialize)]
    struct ModuleEntry {
        #[serde(default)]
        versions: Vec<ModuleVersion>,
    }
    #[derive(serde::Deserialize)]
    struct ModuleVersion {
        version: String,
    }
    let parsed: ModuleVersionsResp =
        serde_json::from_str(&body).map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    Ok(parsed
        .modules
        .into_iter()
        .flat_map(|m| m.versions.into_iter().map(|v| v.version))
        .collect())
}

fn module_versions_cache_path(
    registry: &str,
    namespace: &str,
    name: &str,
    provider: &str,
) -> PathBuf {
    cache_root()
        .join("modules")
        .join(sanitise(registry))
        .join(sanitise(namespace))
        .join(sanitise(name))
        .join(sanitise(provider))
        .join("versions.json")
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

    #[test]
    fn merge_tags_shared_versions_with_both_registries() {
        let tf = vec!["5.99.0".to_string(), "5.98.0".to_string(), "5.97.0".to_string()];
        let tofu = vec!["5.99.0".to_string(), "5.97.0".to_string()];
        let merged = merge_with_provenance(tf, tofu);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].version, "5.99.0");
        assert_eq!(merged[0].registries, vec![Registry::Terraform, Registry::OpenTofu]);
        assert_eq!(merged[0].provenance_label(), "terraform + opentofu");
        assert_eq!(merged[1].version, "5.98.0");
        assert_eq!(merged[1].registries, vec![Registry::Terraform]);
        assert_eq!(merged[1].provenance_label(), "terraform only");
        assert_eq!(merged[2].version, "5.97.0");
        assert_eq!(merged[2].registries, vec![Registry::Terraform, Registry::OpenTofu]);
    }

    #[test]
    fn merge_appends_opentofu_only_versions() {
        let tf = vec!["5.99.0".to_string()];
        let tofu = vec!["5.99.0".to_string(), "5.99.0-opentofu-fork".to_string()];
        let merged = merge_with_provenance(tf, tofu);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[1].version, "5.99.0-opentofu-fork");
        assert_eq!(merged[1].registries, vec![Registry::OpenTofu]);
        assert_eq!(merged[1].provenance_label(), "opentofu only");
    }

    #[test]
    fn merge_handles_one_registry_empty() {
        let tf: Vec<String> = Vec::new();
        let tofu = vec!["1.2.3".to_string()];
        let merged = merge_with_provenance(tf, tofu);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].registries, vec![Registry::OpenTofu]);
    }

    #[test]
    fn merge_sorts_semver_descending_regardless_of_input_order() {
        // Inputs are intentionally scrambled so the sort — not the
        // input order — has to produce newest-first.
        let tf = vec![
            "1.2.3".to_string(),
            "10.0.0".to_string(),
            "2.0.0".to_string(),
            "1.2.10".to_string(),
        ];
        let tofu: Vec<String> = Vec::new();
        let merged = merge_with_provenance(tf, tofu);
        let labels: Vec<_> = merged.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(labels, vec!["10.0.0", "2.0.0", "1.2.10", "1.2.3"]);
    }

    #[test]
    fn merge_places_stable_before_prerelease_at_same_core() {
        let tf = vec![
            "1.0.0-rc2".to_string(),
            "1.0.0".to_string(),
            "1.0.0-alpha".to_string(),
        ];
        let merged = merge_with_provenance(tf, Vec::new());
        let labels: Vec<_> = merged.iter().map(|v| v.version.as_str()).collect();
        // Stable wins over both pre-releases; rc2 > alpha lexically.
        assert_eq!(labels, vec!["1.0.0", "1.0.0-rc2", "1.0.0-alpha"]);
    }

    #[test]
    fn merge_puts_unparseable_at_the_end() {
        let tf = vec![
            "nightly".to_string(),
            "1.2.3".to_string(),
            "2.0.0".to_string(),
        ];
        let merged = merge_with_provenance(tf, Vec::new());
        let labels: Vec<_> = merged.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(labels, vec!["2.0.0", "1.2.3", "nightly"]);
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
