//! On-disk cache + bundled fallback for the Terraform/OpenTofu
//! built-in function signatures.
//!
//! Lookup order:
//!   1. In-memory cache (outer caller holds this via `StateStore`).
//!   2. On-disk XDG cache (`$XDG_CACHE_HOME/terraform-ls-rs/functions/*`).
//!      Key: sha256 of the binary's canonical path + its mtime. A
//!      binary upgrade invalidates the cache automatically.
//!   3. CLI invocation (`<binary> metadata functions -json`).
//!   4. Bundled snapshot (always available).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::io::Read;

use crate::error::SchemaError;
use crate::fetcher::fetch_functions_from_cli;
use crate::functions::FunctionsSchema;

const BUNDLED: &[u8] = include_bytes!("../../../schemas/functions.opentofu.json.gz");
const CLI_TIMEOUT_SECS: u64 = 15;

/// Resolve the functions schema for a given binary. Tries on-disk
/// cache first, then CLI, then the bundled snapshot.
///
/// Never fails: the bundled snapshot is the infallible last resort.
pub async fn load_functions(binary: &Path) -> FunctionsSchema {
    // 1. Disk cache.
    if let Some(key) = cache_key(binary) {
        if let Some(schema) = read_disk_cache(&key) {
            tracing::debug!(binary = %binary.display(), "functions loaded from disk cache");
            return schema;
        }
    }

    // 2. CLI.
    match fetch_functions_from_cli(
        binary,
        std::time::Duration::from_secs(CLI_TIMEOUT_SECS),
    )
    .await
    {
        Ok(schema) => {
            if let Some(key) = cache_key(binary) {
                if let Err(e) = write_disk_cache(&key, &schema) {
                    tracing::warn!(error = %e, "failed to write functions cache");
                }
            }
            tracing::info!(binary = %binary.display(), "functions fetched via CLI");
            return schema;
        }
        Err(e) => {
            tracing::debug!(error = %e, "CLI function fetch failed, using bundled snapshot");
        }
    }

    // 3. Bundled fallback.
    bundled().unwrap_or_else(|e| {
        tracing::error!(error = %e, "bundled functions snapshot failed; returning empty schema");
        FunctionsSchema {
            format_version: String::new(),
            function_signatures: Default::default(),
        }
    })
}

/// Decode the compiled-in bundled snapshot.
pub fn bundled() -> Result<FunctionsSchema, SchemaError> {
    let mut decoder = GzDecoder::new(BUNDLED);
    let mut json = String::new();
    decoder
        .read_to_string(&mut json)
        .map_err(|source| SchemaError::Decompression {
            name: "functions.opentofu".to_string(),
            source,
        })?;
    sonic_rs::from_str(&json).map_err(SchemaError::JsonParse)
}

/// A stable cache key: sha256 of (canonical path bytes || mtime
/// seconds). Returns `None` if we can't stat the binary.
fn cache_key(binary: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(binary).ok()?;
    let mtime = std::fs::metadata(&canonical)
        .ok()?
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();

    let mut hasher = Sha256::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    hasher.update(mtime.to_le_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache"))
        })?;
    Some(base.join("terraform-ls-rs").join("functions"))
}

fn cache_path(key: &str) -> Option<PathBuf> {
    cache_dir().map(|d| d.join(format!("{key}.json")))
}

fn read_disk_cache(key: &str) -> Option<FunctionsSchema> {
    let path = cache_path(key)?;
    let bytes = std::fs::read(path).ok()?;
    sonic_rs::from_slice(&bytes).ok()
}

fn write_disk_cache(key: &str, schema: &FunctionsSchema) -> Result<(), SchemaError> {
    let dir = cache_dir().ok_or_else(|| {
        SchemaError::Cache(std::io::Error::other("no cache directory available"))
    })?;
    std::fs::create_dir_all(&dir).map_err(SchemaError::Cache)?;
    let path = dir.join(format!("{key}.json"));
    let json = sonic_rs::to_string(schema).map_err(SchemaError::JsonParse)?;
    std::fs::write(path, json).map_err(SchemaError::Cache)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn bundled_snapshot_has_common_functions() {
        let schema = bundled().expect("bundled");
        assert!(schema.function_signatures.contains_key("abs"));
        assert!(schema.function_signatures.contains_key("format"));
        assert!(schema.function_signatures.contains_key("jsonencode"));
        assert!(schema.function_signatures.contains_key("lookup"));
    }

    #[test]
    fn cache_key_is_stable_across_calls() {
        // Use the current test binary — it definitely exists and is stable.
        let exe = std::env::current_exe().expect("current exe");
        let k1 = cache_key(&exe).expect("key");
        let k2 = cache_key(&exe).expect("key");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64); // sha256 hex
    }

    #[test]
    fn cache_key_is_none_for_missing_binary() {
        let missing = PathBuf::from("/definitely/not/a/real/path/xyz123");
        assert!(cache_key(&missing).is_none());
    }

    #[tokio::test]
    async fn load_functions_falls_back_to_bundled_when_cli_missing() {
        // Use a bogus binary path — disk cache will also miss.
        let schema = load_functions(Path::new("/nonexistent-tfls-binary")).await;
        assert!(schema.function_signatures.contains_key("abs"));
    }
}
