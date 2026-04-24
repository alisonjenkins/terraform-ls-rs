//! Fetch attribute descriptions from the Terraform Registry.
//!
//! Many SDKv2-based providers (notably `hashicorp/aws`) don't include
//! per-attribute descriptions in the gRPC schema. The Registry, however,
//! hosts the hand-written markdown docs they generate from their
//! `website/docs/` tree. We fetch those, parse the `## Argument Reference`
//! and `## Attribute Reference` sections, and feed the descriptions back
//! into the schema so hover/completion have something to show.
//!
//! All HTTP responses are cached to disk under
//! `$XDG_CACHE_HOME/terraform-ls-rs/provider-docs/...` so repeated runs
//! are ~free.
//!
//! The parser targets the two common doc formats:
//!
//! 1. **tfplugindocs-generated** (modern, schema-driven):
//!    `### Required\n\n- `attr_name` (Type) description.`
//! 2. **hand-written** (SDKv2 classic, e.g. AWS):
//!    `* `attr_name` - (Required) description.`
//!
//! Both are handled by the same regex pass over the relevant section.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use regex::Regex;
use serde::Deserialize;

use crate::ProtocolError;

/// Registry host for `hashicorp/*` and most community providers.
const REGISTRY_HOST: &str = "https://registry.terraform.io";
/// Upper bound on concurrent per-resource doc fetches. AWS has ~1500
/// resources; going higher flirts with rate limits and doesn't help
/// latency much since the disk cache kicks in on the second run.
const FETCH_CONCURRENCY: usize = 8;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// A single entry in the `/v1/providers/...` doc index.
#[derive(Debug, Clone, Deserialize)]
struct IndexDocEntry {
    id: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    language: String,
}

#[derive(Debug, Clone, Deserialize)]
struct IndexResponse {
    #[serde(default)]
    docs: Vec<IndexDocEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct DocResponse {
    data: DocData,
}
#[derive(Debug, Clone, Deserialize)]
struct DocData {
    attributes: DocAttributes,
}
#[derive(Debug, Clone, Deserialize)]
struct DocAttributes {
    #[serde(default)]
    content: String,
}

/// The subset of the registry index we care about: a mapping from
/// `(category, slug)` → doc id that can be fetched individually.
#[derive(Debug, Clone, Default)]
pub struct ProviderDocIndex {
    /// Keyed by `"resources:slug"` or `"data-sources:slug"`.
    pub entries: HashMap<String, String>,
}

impl ProviderDocIndex {
    pub fn get_resource(&self, slug: &str) -> Option<&str> {
        self.entries
            .get(&format!("resources:{slug}"))
            .map(String::as_str)
    }

    pub fn get_data_source(&self, slug: &str) -> Option<&str> {
        self.entries
            .get(&format!("data-sources:{slug}"))
            .map(String::as_str)
    }
}

/// Build a persistent reqwest client suitable for registry access.
fn build_http_client() -> Result<reqwest::Client, ProtocolError> {
    reqwest::Client::builder()
        .user_agent("terraform-ls-rs/0.1 (+registry-docs)")
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))
}

/// Resolve the on-disk cache directory for provider docs.
///
/// Cross-platform via the `dirs` crate:
/// - Linux: `$XDG_CACHE_HOME/terraform-ls-rs/provider-docs`
///   (fallback `~/.cache/terraform-ls-rs/provider-docs`)
/// - macOS: `~/Library/Caches/terraform-ls-rs/provider-docs`
/// - Windows: `%LOCALAPPDATA%\terraform-ls-rs\provider-docs`
///
/// Falls back to `std::env::temp_dir()` if the platform dir is
/// unavailable (e.g. running under a minimal container without
/// `HOME` set).
fn cache_root() -> PathBuf {
    if let Some(base) = dirs::cache_dir() {
        return base.join("terraform-ls-rs").join("provider-docs");
    }
    std::env::temp_dir()
        .join("terraform-ls-rs")
        .join("provider-docs")
}

/// Version tag for the parsed-descriptions cache. Bump when the
/// on-disk format changes incompatibly so stale caches don't cause
/// subtle miscompares or panics on deserialize.
const PARSED_CACHE_VERSION: u32 = 1;

/// Parsed-descriptions cache: the consolidated output of running
/// enrichment for one (namespace/name/version) tuple, keyed in a
/// way that lets subsequent runs skip both HTTP *and* markdown
/// parsing — only a single JSON read + in-memory merge.
#[derive(Debug, serde::Serialize, Deserialize)]
struct ParsedDocsCache {
    cache_version: u32,
    namespace: String,
    name: String,
    version: String,
    /// resource_type → (attribute_name → description)
    resources: HashMap<String, HashMap<String, String>>,
    /// data_source_type → (attribute_name → description)
    data_sources: HashMap<String, HashMap<String, String>>,
}

fn parsed_cache_path(namespace: &str, name: &str, version: &str) -> PathBuf {
    cache_root()
        .join(sanitise(namespace))
        .join(sanitise(name))
        .join(sanitise(version))
        .join("parsed-descriptions.json")
}

async fn read_parsed_cache(path: &Path) -> Option<ParsedDocsCache> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    let parsed: ParsedDocsCache = serde_json::from_str(&text).ok()?;
    if parsed.cache_version != PARSED_CACHE_VERSION {
        return None;
    }
    Some(parsed)
}

async fn write_parsed_cache(path: &Path, entry: &ParsedDocsCache) {
    let json = match serde_json::to_string(entry) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "parsed cache serialize failed");
            return;
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::debug!(error = %e, dir = %parent.display(), "parsed cache dir create failed");
            return;
        }
    }
    // Atomic-ish write: tmp file + rename so a kill mid-write doesn't
    // leave a half-written cache that fails to deserialize later.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = tokio::fs::write(&tmp, json).await {
        tracing::debug!(error = %e, path = %tmp.display(), "parsed cache tmp write failed");
        return;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        tracing::debug!(error = %e, path = %path.display(), "parsed cache rename failed");
        let _ = tokio::fs::remove_file(&tmp).await;
    }
}

fn sanitise(component: &str) -> String {
    component
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' { c } else { '_' })
        .collect()
}

fn index_cache_path(namespace: &str, name: &str, version: &str) -> PathBuf {
    cache_root()
        .join(sanitise(namespace))
        .join(sanitise(name))
        .join(sanitise(version))
        .join("index.json")
}

fn doc_cache_path(namespace: &str, name: &str, version: &str, doc_id: &str) -> PathBuf {
    cache_root()
        .join(sanitise(namespace))
        .join(sanitise(name))
        .join(sanitise(version))
        .join("docs")
        .join(format!("{}.md", sanitise(doc_id)))
}

async fn read_cache(path: &Path) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}

async fn write_cache(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::debug!(error = %e, dir = %parent.display(), "cache dir create failed");
            return;
        }
    }
    if let Err(e) = tokio::fs::write(path, contents).await {
        tracing::debug!(error = %e, path = %path.display(), "cache write failed");
    }
}

/// Fetch the doc index for `<namespace>/<name>@<version>` from the
/// registry. Results are cached to disk — repeat calls are free.
pub async fn fetch_index(
    client: &reqwest::Client,
    namespace: &str,
    name: &str,
    version: &str,
) -> Result<ProviderDocIndex, ProtocolError> {
    let cache_path = index_cache_path(namespace, name, version);
    let body = if let Some(cached) = read_cache(&cache_path).await {
        cached
    } else {
        let url = format!("{REGISTRY_HOST}/v1/providers/{namespace}/{name}/{version}");
        tracing::debug!(%url, "fetching registry doc index");
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ProtocolError::RegistryHttp(format!(
                "index {url} returned {status}"
            )));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
        write_cache(&cache_path, &text).await;
        text
    };

    let parsed: IndexResponse = serde_json::from_str(&body)
        .map_err(|e| ProtocolError::RegistryParse(format!("index: {e}")))?;

    let mut entries = HashMap::with_capacity(parsed.docs.len());
    for d in parsed.docs {
        // Only the `hcl` language variant — cdktf python/typescript/etc.
        // are duplicates of the same doc content and would overwrite
        // the hcl entry with a language-specific copy.
        if !d.language.is_empty() && d.language != "hcl" {
            continue;
        }
        if d.category != "resources" && d.category != "data-sources" {
            continue;
        }
        entries.insert(format!("{}:{}", d.category, d.slug), d.id);
    }
    Ok(ProviderDocIndex { entries })
}

/// Fetch the markdown content for a single doc id.
pub async fn fetch_doc_content(
    client: &reqwest::Client,
    namespace: &str,
    name: &str,
    version: &str,
    doc_id: &str,
) -> Result<String, ProtocolError> {
    let cache_path = doc_cache_path(namespace, name, version, doc_id);
    if let Some(cached) = read_cache(&cache_path).await {
        return Ok(cached);
    }
    let url = format!("{REGISTRY_HOST}/v2/provider-docs/{doc_id}");
    tracing::debug!(%url, "fetching registry doc content");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ProtocolError::RegistryHttp(format!(
            "doc {url} returned {status}"
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| ProtocolError::RegistryHttp(e.to_string()))?;
    let parsed: DocResponse = serde_json::from_str(&body)
        .map_err(|e| ProtocolError::RegistryParse(format!("doc: {e}")))?;
    let content = parsed.data.attributes.content;
    write_cache(&cache_path, &content).await;
    Ok(content)
}

/// Parse `* `name` - (Required) description` and
/// `- `name` (Type) description` style bullets out of the
/// `Argument Reference` / `Attribute Reference` sections.
///
/// Returns a map from attribute name → description string.
pub fn parse_attribute_descriptions(markdown: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();

    // Split markdown into top-level (`##`) sections. We only care about
    // the Argument/Attribute Reference sections — the rest of the doc
    // is example usage, imports, etc.
    let mut in_section = false;
    // Collected text per attr: we accumulate continuation lines until a
    // blank line or a new bullet appears, so multi-line descriptions
    // survive intact.
    let mut current: Option<(String, String)> = None;

    // `* \`name\` - (qualifiers) rest`  OR  `- \`name\` (Type) rest`
    // Qualifiers (Required)/(Optional)/etc. are stripped separately after
    // the initial match so both formats land in the same shape.
    let bullet = Regex::new(
        r"^\s*[-*]\s+`([A-Za-z_][A-Za-z0-9_]*)`\s*(?:\(([^)]+)\))?\s*(?:[-—:]\s*)?(.*)$",
    )
    .ok();

    let Some(bullet) = bullet else {
        return out;
    };

    let h2_start = Regex::new(r"^##\s+(.*)$").ok();
    let Some(h2_start) = h2_start else {
        return out;
    };

    // Keep the *first* occurrence of an attribute name. AWS SDKv2 docs
    // list many nested blocks with reused attribute names (e.g. `bucket`
    // appears in the top-level args and again under `destination` inside
    // replication rules). The top-level description is almost always the
    // correct one for the top-level attribute, so we preserve it.
    let flush = |cur: &mut Option<(String, String)>, out: &mut HashMap<String, String>| {
        if let Some((k, v)) = cur.take() {
            let v = v.trim().trim_end_matches('.').to_string();
            if !v.is_empty() {
                out.entry(k).or_insert(v);
            }
        }
    };

    for line in markdown.lines() {
        if let Some(h) = h2_start.captures(line).and_then(|c| c.get(1)) {
            flush(&mut current, &mut out);
            let title = h.as_str().to_ascii_lowercase();
            in_section = title.contains("argument reference")
                || title.contains("attribute reference")
                || title.contains("schema")
                || title.contains("nested schema");
            continue;
        }
        if !in_section {
            continue;
        }

        if let Some(caps) = bullet.captures(line) {
            flush(&mut current, &mut out);
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or_default().to_string();
            let qualifiers = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
            let rest = caps.get(3).map(|m| m.as_str().trim()).unwrap_or_default();

            // If the qualifier looks like a cty type (e.g. `String`,
            // `Map of String`, `List of Object`), drop it — it's not
            // meaningful for hover text, and the type is already in
            // the schema. Keep Required/Optional/etc. qualifiers as-is.
            let is_type_like = qualifiers
                .split([',', ' ', '/'])
                .next()
                .map(|first| {
                    matches!(
                        first.trim(),
                        "String"
                            | "Number"
                            | "Bool"
                            | "Boolean"
                            | "List"
                            | "Map"
                            | "Set"
                            | "Object"
                            | "Block"
                            | "Block List"
                            | "Block Set"
                    )
                })
                .unwrap_or(false);

            let mut buf = String::new();
            if !qualifiers.is_empty() && !is_type_like {
                buf.push('(');
                buf.push_str(qualifiers.trim());
                buf.push(')');
                buf.push(' ');
            }
            buf.push_str(rest);
            current = Some((name, buf));
            continue;
        }

        // Continuation: a blank line flushes; a non-bullet non-blank
        // line gets appended to the current description (tfplugindocs
        // sometimes wraps long descriptions across lines).
        if line.trim().is_empty() {
            flush(&mut current, &mut out);
            continue;
        }
        if let Some((_, buf)) = current.as_mut() {
            if !buf.is_empty() {
                buf.push(' ');
            }
            buf.push_str(line.trim());
        }
    }
    flush(&mut current, &mut out);
    out
}

/// Merge registry-fetched descriptions into an existing
/// [`tfls_schema::ProviderSchemas`] in-place.
///
/// Only attributes whose current `description` is `None` or empty are
/// overwritten; we don't want to squash real per-attribute docs when a
/// provider already ships them. Any resource whose doc can't be fetched
/// is skipped with a debug log — we always return `Ok(count)` where
/// `count` is the number of attributes that got a new description.
pub async fn enrich_schemas_with_registry_docs(
    schemas: &mut tfls_schema::ProviderSchemas,
    providers: &[ProviderCoords],
) -> Result<usize, ProtocolError> {
    let client = Arc::new(build_http_client()?);
    let mut total_updated = 0usize;

    for pc in providers {
        let provider_start = std::time::Instant::now();

        let Some(provider_schema) = schemas.provider_schemas.get_mut(&pc.address) else {
            continue;
        };

        // Fast path: parsed-descriptions cache. Skips both HTTP and
        // markdown-regex parsing (which is meaningful for big
        // providers — aws_has 2k+ resources × regex-per-doc on the
        // hot path otherwise).
        let cache_path = parsed_cache_path(&pc.namespace, &pc.name, &pc.version);
        if let Some(cached) = read_parsed_cache(&cache_path).await {
            let mut provider_updated = 0usize;
            for (type_name, descriptions) in &cached.resources {
                if let Some(s) = provider_schema.resource_schemas.get_mut(type_name) {
                    provider_updated += merge_descriptions_into_block(&mut s.block, descriptions);
                }
            }
            for (type_name, descriptions) in &cached.data_sources {
                if let Some(s) = provider_schema.data_source_schemas.get_mut(type_name) {
                    provider_updated += merge_descriptions_into_block(&mut s.block, descriptions);
                }
            }
            tracing::info!(
                provider = %format!("{}/{}@{}", pc.namespace, pc.name, pc.version),
                updated = provider_updated,
                elapsed_ms = provider_start.elapsed().as_millis() as u64,
                "registry enrichment complete (parsed cache hit)"
            );
            total_updated += provider_updated;
            continue;
        }

        let index = match fetch_index(&client, &pc.namespace, &pc.name, &pc.version).await {
            Ok(i) => i,
            Err(e) => {
                tracing::info!(
                    error = %e,
                    provider = %format!("{}/{}@{}", pc.namespace, pc.name, pc.version),
                    "skipping registry enrichment (index unavailable)"
                );
                continue;
            }
        };

        // Collect the set of (kind, type, doc_id) to fetch, restricted
        // to resources whose current schemas have at least one
        // description-less attribute (no point hammering the registry
        // for providers that already ship descriptions).
        #[derive(Clone, Copy)]
        enum Kind {
            Resource,
            DataSource,
        }
        let mut targets: Vec<(Kind, String, String)> = Vec::new();

        for (type_name, schema) in &provider_schema.resource_schemas {
            if !schema_has_missing_descriptions(schema) {
                continue;
            }
            let Some(id) = strip_provider_prefix(&pc.name, type_name)
                .and_then(|slug| index.get_resource(slug))
            else {
                continue;
            };
            targets.push((Kind::Resource, type_name.clone(), id.to_string()));
        }
        for (type_name, schema) in &provider_schema.data_source_schemas {
            if !schema_has_missing_descriptions(schema) {
                continue;
            }
            let Some(id) = strip_provider_prefix(&pc.name, type_name)
                .and_then(|slug| index.get_data_source(slug))
            else {
                continue;
            };
            targets.push((Kind::DataSource, type_name.clone(), id.to_string()));
        }

        tracing::info!(
            provider = %format!("{}/{}@{}", pc.namespace, pc.name, pc.version),
            count = targets.len(),
            "enriching schemas from registry"
        );

        // Fetch doc content with bounded concurrency.
        let ns = pc.namespace.clone();
        let name = pc.name.clone();
        let version = pc.version.clone();
        let fetches = stream::iter(targets.into_iter().map(|(kind, type_name, id)| {
            let client = Arc::clone(&client);
            let ns = ns.clone();
            let name = name.clone();
            let version = version.clone();
            async move {
                let content = fetch_doc_content(&client, &ns, &name, &version, &id).await;
                (kind, type_name, id, content)
            }
        }))
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

        let mut provider_updated = 0usize;
        // Accumulate the parsed descriptions so we can write them to
        // the parsed-cache after the loop — next run skips all of
        // this.
        let mut cache_entry = ParsedDocsCache {
            cache_version: PARSED_CACHE_VERSION,
            namespace: pc.namespace.clone(),
            name: pc.name.clone(),
            version: pc.version.clone(),
            resources: HashMap::new(),
            data_sources: HashMap::new(),
        };
        for (kind, type_name, id, result) in fetches {
            let content = match result {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        resource = %type_name,
                        doc_id = %id,
                        "failed to fetch doc"
                    );
                    continue;
                }
            };
            let descriptions = parse_attribute_descriptions(&content);
            if descriptions.is_empty() {
                continue;
            }
            let schema_entry = match kind {
                Kind::Resource => provider_schema.resource_schemas.get_mut(&type_name),
                Kind::DataSource => provider_schema.data_source_schemas.get_mut(&type_name),
            };
            let Some(schema) = schema_entry else {
                continue;
            };
            provider_updated += merge_descriptions_into_block(&mut schema.block, &descriptions);
            match kind {
                Kind::Resource => {
                    cache_entry.resources.insert(type_name, descriptions);
                }
                Kind::DataSource => {
                    cache_entry.data_sources.insert(type_name, descriptions);
                }
            }
        }

        // Write the consolidated cache for next run. Fire-and-forget;
        // write errors are logged at debug level and don't block the
        // enrichment pass.
        if !cache_entry.resources.is_empty() || !cache_entry.data_sources.is_empty() {
            write_parsed_cache(&cache_path, &cache_entry).await;
        }

        tracing::info!(
            provider = %format!("{}/{}@{}", pc.namespace, pc.name, pc.version),
            updated = provider_updated,
            elapsed_ms = provider_start.elapsed().as_millis() as u64,
            "registry enrichment complete"
        );
        total_updated += provider_updated;
    }

    Ok(total_updated)
}

/// Coordinates needed to fetch a provider's docs from the registry.
#[derive(Debug, Clone)]
pub struct ProviderCoords {
    pub address: String,
    pub namespace: String,
    pub name: String,
    pub version: String,
}

/// Resources on the registry are listed by their short slug
/// (`instance`, `s3_bucket`) not their namespaced type name
/// (`aws_instance`). Strip the provider prefix.
fn strip_provider_prefix<'a>(provider_name: &str, type_name: &'a str) -> Option<&'a str> {
    let prefix = format!("{provider_name}_");
    type_name.strip_prefix(&prefix)
}

fn schema_has_missing_descriptions(schema: &tfls_schema::Schema) -> bool {
    schema
        .block
        .attributes
        .values()
        .any(|a| a.description.as_deref().map(str::is_empty).unwrap_or(true))
}

/// Copy descriptions into empty slots of `block`'s attributes. Does NOT
/// recurse into nested blocks: AWS SDKv2 docs reuse the same attribute
/// name across many nested blocks (e.g. `bucket` appears top-level and
/// again under replication-rule destinations), and naively propagating
/// the top-level description into every nested block would produce
/// misleading hover text. Nested attributes stay description-less —
/// that's no worse than the pre-enrichment state.
fn merge_descriptions_into_block(
    block: &mut tfls_schema::BlockSchema,
    descriptions: &HashMap<String, String>,
) -> usize {
    let mut updated = 0;
    for (attr_name, attr) in block.attributes.iter_mut() {
        if attr.description.as_deref().map(str::is_empty).unwrap_or(true) {
            if let Some(desc) = descriptions.get(attr_name) {
                attr.description = Some(desc.clone());
                updated += 1;
            }
        }
    }
    updated
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_sdkv2_argument_reference() {
        let md = r#"
# Resource: aws_ses_domain_identity

Stuff.

## Argument Reference

This resource supports the following arguments:

* `domain` - (Required) The domain name to assign to SES
* `other` - (Optional) Another thing. Spans
  across two lines.

## Attribute Reference

* `arn` - The ARN of the thing.
"#;
        let descs = parse_attribute_descriptions(md);
        assert_eq!(
            descs.get("domain").map(String::as_str),
            Some("(Required) The domain name to assign to SES")
        );
        assert!(
            descs.get("other").unwrap().contains("across two lines"),
            "multi-line continuation: {:?}",
            descs.get("other")
        );
        assert_eq!(
            descs.get("arn").map(String::as_str),
            Some("The ARN of the thing")
        );
    }

    #[test]
    fn parses_tfplugindocs_schema_section() {
        let md = r#"
## Schema

### Required

- `region` (String) AWS region name.

### Optional

- `profile` (String) Named AWS profile.
"#;
        let descs = parse_attribute_descriptions(md);
        assert_eq!(
            descs.get("region").map(String::as_str),
            Some("AWS region name")
        );
        assert_eq!(
            descs.get("profile").map(String::as_str),
            Some("Named AWS profile")
        );
    }

    #[test]
    fn strips_provider_prefix_works() {
        assert_eq!(strip_provider_prefix("aws", "aws_instance"), Some("instance"));
        assert_eq!(strip_provider_prefix("aws", "other_thing"), None);
    }

    #[test]
    fn merge_fills_only_missing() {
        use tfls_schema::{AttributeSchema, BlockSchema};
        let mut block = BlockSchema::default();
        block.attributes.insert(
            "a".into(),
            AttributeSchema {
                description: None,
                ..Default::default()
            },
        );
        block.attributes.insert(
            "b".into(),
            AttributeSchema {
                description: Some("already here".into()),
                ..Default::default()
            },
        );
        let mut descs = HashMap::new();
        descs.insert("a".into(), "from registry".into());
        descs.insert("b".into(), "should not overwrite".into());

        let updated = merge_descriptions_into_block(&mut block, &descs);
        assert_eq!(updated, 1);
        assert_eq!(
            block.attributes.get("a").unwrap().description.as_deref(),
            Some("from registry")
        );
        assert_eq!(
            block.attributes.get("b").unwrap().description.as_deref(),
            Some("already here")
        );
    }
}
