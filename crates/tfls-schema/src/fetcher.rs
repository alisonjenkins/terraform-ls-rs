//! Async schema fetcher: invokes `terraform providers schema -json`
//! (or `tofu providers schema -json`) via `tokio::process` and parses
//! the output with `sonic-rs`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::error::SchemaError;
use crate::functions::FunctionsSchema;
use crate::types::ProviderSchemas;

const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Configuration for invoking the Terraform/OpenTofu CLI.
#[derive(Debug, Clone)]
pub struct SchemaFetcher {
    /// Path to the `terraform` or `tofu` binary. Defaults to `tofu`
    /// (OpenTofu, MPL-licensed). Override for terraform proper.
    pub binary: PathBuf,
    /// Working directory to invoke from — should be a directory where
    /// `terraform init` has been run.
    pub working_dir: PathBuf,
    /// Max time to wait for the CLI invocation.
    pub timeout: Duration,
}

impl SchemaFetcher {
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            binary: PathBuf::from("tofu"),
            working_dir: working_dir.into(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    pub fn with_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.binary = path.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Invoke the CLI and parse the JSON output.
    pub async fn fetch(&self) -> Result<ProviderSchemas, SchemaError> {
        fetch_schema_from_cli(&self.binary, &self.working_dir, self.timeout).await
    }
}

/// Run `<binary> providers schema -json` in `working_dir` and parse
/// the stdout as [`ProviderSchemas`]. The process is killed if it
/// exceeds `timeout`.
pub async fn fetch_schema_from_cli(
    binary: &Path,
    working_dir: &Path,
    timeout: Duration,
) -> Result<ProviderSchemas, SchemaError> {
    tracing::debug!(binary = %binary.display(), dir = %working_dir.display(), "fetching provider schemas");

    let mut cmd = Command::new(binary);
    cmd.args(["providers", "schema", "-json"])
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd.spawn().map_err(SchemaError::CliExecution)?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(source)) => return Err(SchemaError::CliExecution(source)),
        Err(_) => {
            return Err(SchemaError::CliTimeout {
                timeout_secs: timeout.as_secs(),
            });
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(SchemaError::CliFailed {
            status: output.status.code().unwrap_or(-1),
            stderr,
        });
    }

    let schemas: ProviderSchemas =
        sonic_rs::from_slice(&output.stdout).map_err(SchemaError::JsonParse)?;
    Ok(schemas)
}

/// Run `<binary> metadata functions -json` and parse the output.
/// Does not require a `working_dir` — the subcommand works without
/// `terraform init`.
pub async fn fetch_functions_from_cli(
    binary: &Path,
    timeout: Duration,
) -> Result<FunctionsSchema, SchemaError> {
    tracing::debug!(binary = %binary.display(), "fetching functions metadata");

    let mut cmd = Command::new(binary);
    cmd.args(["metadata", "functions", "-json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd.spawn().map_err(SchemaError::CliExecution)?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(source)) => return Err(SchemaError::CliExecution(source)),
        Err(_) => {
            return Err(SchemaError::CliTimeout {
                timeout_secs: timeout.as_secs(),
            });
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(SchemaError::CliFailed {
            status: output.status.code().unwrap_or(-1),
            stderr,
        });
    }

    sonic_rs::from_slice(&output.stdout).map_err(SchemaError::JsonParse)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_binary_reports_cli_execution_error() {
        let fetcher = SchemaFetcher::new(std::env::temp_dir())
            .with_binary("definitely-not-a-real-cli-binary-xyz-12345")
            .with_timeout(Duration::from_secs(2));

        let err = fetcher.fetch().await;
        assert!(
            matches!(err, Err(SchemaError::CliExecution(_))),
            "expected CliExecution, got {err:?}"
        );
    }

    #[test]
    fn fetcher_defaults_to_tofu_binary() {
        let f = SchemaFetcher::new(std::env::temp_dir());
        assert_eq!(f.binary, PathBuf::from("tofu"));
        assert_eq!(f.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }
}
