//! Speaks the terraform plugin gRPC protocol directly to cached provider
//! binaries.
//!
//! Bypasses `tofu providers schema -json` (which requires backend init
//! and credentials) by launching the provider binary, doing the
//! go-plugin handshake, and calling the `GetProviderSchema` /
//! `GetFunctions` RPCs over mTLS.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::result_large_err)]

use std::path::Path;

pub mod discovery;
pub mod handshake;
pub mod tls;
pub mod translate;
pub mod client;

#[allow(dead_code, clippy::all)]
pub(crate) mod proto {
    tonic::include_proto!("tfplugin6");
}

pub use discovery::{ProviderBinary, discover_providers};
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
}

/// Discover and fetch provider schemas for every provider cached under
/// `<terraform_dir>/providers/`. Returns a merged [`tfls_schema::ProviderSchemas`]
/// ready to install into `StateStore`.
pub async fn fetch_schemas_from_plugins(
    terraform_dir: &Path,
) -> Result<tfls_schema::ProviderSchemas, ProtocolError> {
    let binaries = discover_providers(terraform_dir)?;
    let mut provider_schemas = std::collections::HashMap::new();

    for bin in binaries {
        match client::fetch_provider_schema(&bin).await {
            Ok(schema) => {
                let key = bin.full_address();
                provider_schemas.insert(key, schema);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    provider = %bin.binary.display(),
                    "failed to fetch schema from provider binary — skipping",
                );
            }
        }
    }

    Ok(tfls_schema::ProviderSchemas {
        format_version: "1.0".to_string(),
        provider_schemas,
    })
}
