//! Speaks the terraform plugin gRPC protocol directly to cached provider
//! binaries.
//!
//! Bypasses `tofu providers schema -json` (which requires backend init
//! and credentials) by launching the provider binary, doing the
//! go-plugin handshake, and calling the `GetProviderSchema` /
//! `GetFunctions` RPCs over mTLS.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::result_large_err)]

use std::path::Path;

pub mod client;
pub mod discovery;
pub mod handshake;
pub mod registry_catalog;
pub mod registry_docs;
pub mod registry_versions;
pub mod tls;
pub mod tool_versions;
pub mod translate;
pub mod translate_v5;

#[allow(dead_code, clippy::all)]
pub(crate) mod proto {
    tonic::include_proto!("tfplugin6");
}

#[allow(dead_code, clippy::all)]
pub(crate) mod proto_v5 {
    tonic::include_proto!("tfplugin5");
}

pub use discovery::{ProviderBinary, dedupe_providers_keep_highest, discover_providers};
pub use handshake::{HandshakeInfo, PluginInstance, spawn_and_handshake};

/// Error type for the protocol crate.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("failed to spawn provider binary {path}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("provider binary {path} produced no handshake line within timeout")]
    HandshakeTimeout { path: String },

    #[error("malformed handshake line from provider {path:?}: {reason}")]
    BadHandshake { path: String, reason: String },

    #[error("unsupported plugin protocol version {version}; only v6 is implemented")]
    UnsupportedProtocol { version: u32 },

    #[error("failed to read from provider {path}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to configure TLS")]
    Tls(#[source] rustls::Error),

    #[error("failed to generate ephemeral client certificate")]
    CertGen(#[from] rcgen::Error),

    #[error("gRPC transport error talking to provider {path}")]
    Transport {
        path: String,
        #[source]
        source: tonic::transport::Error,
    },

    #[error("gRPC call to provider {path} failed: {status}")]
    Rpc {
        path: String,
        status: tonic::Status,
    },

    #[error("failed to decode MessagePack cty type: {0}")]
    CtyDecode(String),

    #[error("I/O error enumerating provider cache")]
    Discovery(#[source] std::io::Error),

    #[error("registry HTTP error: {0}")]
    RegistryHttp(String),

    #[error("failed to parse registry response: {0}")]
    RegistryParse(String),
}

/// Schema-fetch progress callback: called with
/// `(provider_address, completed_count, total_count)` after each
/// provider binary's schema has been collected (successfully or
/// unsuccessfully). Useful for driving a per-provider LSP progress
/// widget — `completed_count / total_count` renders as a %.
///
/// `Arc<dyn ...>` so the same callback can be cloned cheaply and
/// handed to the inner parallel-fetch loop.
pub type SchemaProgressCallback =
    std::sync::Arc<dyn Fn(&str, usize, usize) + Send + Sync>;

/// Output of the bare plugin-protocol schema fetch — before the
/// (potentially slow) registry-documentation enrichment runs.
pub struct RawPluginSchemas {
    pub schemas: tfls_schema::ProviderSchemas,
    /// Coords to pass to
    /// [`registry_docs::enrich_schemas_with_registry_docs`] if the
    /// caller wants descriptions filled in.
    pub coords: Vec<registry_docs::ProviderCoords>,
}

/// Fetch provider schemas via the plugin gRPC protocol **without**
/// enrichment. Returns as soon as every provider binary has reported
/// its schema, so the caller can install right away and let
/// completion / hover work against the bare structure. Descriptions
/// arrive later by calling
/// [`registry_docs::enrich_schemas_with_registry_docs`] with the
/// returned `coords` — that call takes tens of seconds for big
/// providers (the AWS provider alone has ~2 k resources to enrich)
/// and should run in the background, not on the critical path.
///
/// Fetches across providers run concurrently with a semaphore cap of
/// 8. Each gRPC handshake is independent (separate process spawn +
/// mTLS) so parallelism scales near-linearly until the cap.
pub async fn fetch_schemas_from_plugins_raw(
    terraform_dir: &Path,
    on_progress: Option<SchemaProgressCallback>,
) -> Result<RawPluginSchemas, ProtocolError> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let start = std::time::Instant::now();
    let raw_binaries = discover_providers(terraform_dir)?;
    let discovered = raw_binaries.len();
    // Keep only the highest version of each provider. Terraform's
    // own lock resolver sometimes leaves multiple versions cached
    // together; spawning the gRPC binary for every stale version
    // wastes CPU/RAM (each spawn is ~100 MiB RSS) and the older
    // schemas get overwritten anyway.
    let binaries = dedupe_providers_keep_highest(raw_binaries);
    let total = binaries.len();
    if discovered != total {
        tracing::info!(
            discovered,
            unique_providers = total,
            "plugin schema fetch: deduped older provider versions"
        );
    }
    tracing::info!(total_providers = total, "plugin schema fetch: begin");

    // Generate a single ephemeral mTLS identity up-front and reuse
    // it across every provider connection. RSA-2048 keygen is ~50
    // ms; with one identity per provider that's ~700 ms of pure CPU
    // on a 14-provider workspace that otherwise pegs the fetch
    // time. One session cert works for every provider since each
    // provider only pins the client cert it was handed at spawn.
    let identity = std::sync::Arc::new(tls::ClientIdentity::generate()?);

    // Concurrency cap: 8 is the registry-fetch bound used elsewhere.
    // Each provider binary spawn is ~100 MiB RSS at peak, so this
    // also caps memory during the fetch.
    const FETCH_CONCURRENCY: usize = 8;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(FETCH_CONCURRENCY));

    let mut tasks: FuturesUnordered<_> = binaries
        .into_iter()
        .map(|bin| {
            let sem = std::sync::Arc::clone(&semaphore);
            let identity = std::sync::Arc::clone(&identity);
            async move {
                let _permit = sem.acquire_owned().await.ok()?;
                let fetch_start = std::time::Instant::now();
                let res = client::fetch_provider_schema(&bin, Some(&identity)).await;
                let elapsed_ms = fetch_start.elapsed().as_millis();
                Some((bin, res, elapsed_ms))
            }
        })
        .collect();

    let mut provider_schemas = std::collections::HashMap::new();
    let mut coords: Vec<registry_docs::ProviderCoords> = Vec::new();
    let mut done = 0usize;
    while let Some(item) = tasks.next().await {
        let Some((bin, res, elapsed_ms)) = item else {
            continue;
        };
        done += 1;
        let addr = bin.full_address();
        match res {
            Ok(schema) => {
                tracing::info!(
                    provider = %addr,
                    version = %bin.version,
                    resources = schema.resource_schemas.len(),
                    data_sources = schema.data_source_schemas.len(),
                    elapsed_ms = elapsed_ms as u64,
                    "plugin schema fetched"
                );
                if !provider_schemas.contains_key(&addr) {
                    coords.push(registry_docs::ProviderCoords {
                        address: addr.clone(),
                        namespace: bin.namespace.clone(),
                        name: bin.name.clone(),
                        version: bin.version.clone(),
                    });
                }
                provider_schemas.insert(addr.clone(), schema);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    provider = %bin.binary.display(),
                    elapsed_ms = elapsed_ms as u64,
                    "failed to fetch schema from provider binary — skipping",
                );
            }
        }
        if let Some(cb) = &on_progress {
            cb(&addr, done, total);
        }
    }
    tracing::info!(
        providers = provider_schemas.len(),
        total_ms = start.elapsed().as_millis() as u64,
        "plugin schema fetch: complete"
    );

    Ok(RawPluginSchemas {
        schemas: tfls_schema::ProviderSchemas {
            format_version: "1.0".to_string(),
            provider_schemas,
        },
        coords,
    })
}

/// Convenience wrapper: fetch + enrich in one call. Used by the CLI
/// `fetch_local` / `probe` examples where "blocking is fine". The
/// LSP indexer should use [`fetch_schemas_from_plugins_raw`] +
/// [`registry_docs::enrich_schemas_with_registry_docs`] separately so
/// it can install the bare schemas before the enrichment round-trips
/// finish.
pub async fn fetch_schemas_from_plugins(
    terraform_dir: &Path,
    on_progress: Option<SchemaProgressCallback>,
) -> Result<tfls_schema::ProviderSchemas, ProtocolError> {
    let RawPluginSchemas { mut schemas, coords } =
        fetch_schemas_from_plugins_raw(terraform_dir, on_progress).await?;

    let enrich_start = std::time::Instant::now();
    match registry_docs::enrich_schemas_with_registry_docs(&mut schemas, &coords).await {
        Ok(updated) => {
            tracing::info!(
                attributes_updated = updated,
                elapsed_ms = enrich_start.elapsed().as_millis() as u64,
                "registry enrichment complete",
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                elapsed_ms = enrich_start.elapsed().as_millis() as u64,
                "registry enrichment failed — schemas returned without registry descriptions",
            );
        }
    }

    Ok(schemas)
}
