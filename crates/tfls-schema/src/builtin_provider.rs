//! Bundled snapshot of Terraform's built-in `terraform` provider
//! (`terraform.io/builtin/terraform`).
//!
//! This provider is compiled into Terraform core — it ships no plugin
//! binary under `.terraform/providers/` and is not reliably returned by
//! `terraform providers schema -json` (and never when no providers are
//! installed). Without it the server has no schema for
//! `data "terraform_remote_state"` or `resource "terraform_data"`, so
//! completion and hover for those types come up empty.
//!
//! We carry a compiled-in snapshot and install it once per session, the
//! same way `functions_cache` carries the built-in function signatures.

use std::io::Read;

use flate2::read::GzDecoder;

use crate::error::SchemaError;
use crate::types::ProviderSchemas;

const BUNDLED: &[u8] = include_bytes!("../../../schemas/builtin.terraform.json.gz");

/// Decode the compiled-in built-in provider snapshot.
pub fn bundled() -> Result<ProviderSchemas, SchemaError> {
    let mut decoder = GzDecoder::new(BUNDLED);
    let mut json = String::new();
    decoder
        .read_to_string(&mut json)
        .map_err(|source| SchemaError::Decompression {
            name: "builtin.terraform".to_string(),
            source,
        })?;
    sonic_rs::from_str(&json).map_err(SchemaError::JsonParse)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn bundled_snapshot_has_builtin_types() {
        let schemas = bundled().expect("bundled");
        let provider = schemas
            .provider_schemas
            .get("terraform.io/builtin/terraform")
            .expect("builtin provider present");

        let rs = provider
            .data_source_schemas
            .get("terraform_remote_state")
            .expect("terraform_remote_state present");
        let backend = rs
            .block
            .attributes
            .get("backend")
            .expect("backend attribute present");
        assert!(backend.required);
        assert!(
            backend
                .description
                .as_deref()
                .is_some_and(|d| !d.trim().is_empty()),
            "backend should carry a hover description"
        );

        assert!(
            provider.resource_schemas.contains_key("terraform_data"),
            "terraform_data resource present"
        );
    }
}
