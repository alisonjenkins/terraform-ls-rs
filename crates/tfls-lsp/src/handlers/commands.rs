//! `workspace/executeCommand` — invoke tofu/terraform CLI operations
//! on behalf of the user.
//!
//! Commands accept JSON arguments. Each command runs async under the
//! tokio runtime; on success, returns a status `sonic_rs::Value`; on
//! failure, returns a `jsonrpc::Error`.

use std::path::PathBuf;
use std::time::Duration;

use lsp_types::{ExecuteCommandParams, MessageType};
use tfls_state::{Job, Priority};
use tokio::process::Command;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub const CMD_INIT_WORKSPACE: &str = "terraform-ls-rs.initWorkspace";
pub const CMD_FETCH_SCHEMAS: &str = "terraform-ls-rs.fetchSchemas";
pub const CMD_VALIDATE: &str = "terraform-ls-rs.validate";

pub const COMMANDS: &[&str] = &[CMD_INIT_WORKSPACE, CMD_FETCH_SCHEMAS, CMD_VALIDATE];

pub async fn execute_command(
    backend: &Backend,
    params: ExecuteCommandParams,
) -> jsonrpc::Result<Option<serde_json::Value>> {
    let dir = first_dir_argument(&params.arguments);
    match params.command.as_str() {
        CMD_INIT_WORKSPACE => {
            let dir = dir.ok_or_else(|| missing_arg("working directory"))?;
            run_cli(backend, &dir, &["init", "-backend=false"]).await
        }
        CMD_FETCH_SCHEMAS => {
            let dir = dir.ok_or_else(|| missing_arg("working directory"))?;
            backend.jobs.enqueue(
                Job::FetchSchemas { working_dir: dir },
                Priority::Immediate,
            );
            Ok(Some(serde_json::json!({"enqueued": true})))
        }
        CMD_VALIDATE => {
            let dir = dir.ok_or_else(|| missing_arg("working directory"))?;
            run_cli(backend, &dir, &["validate"]).await
        }
        other => Err(jsonrpc::Error::invalid_params(format!(
            "unknown command: {other}"
        ))),
    }
}

fn first_dir_argument(args: &[serde_json::Value]) -> Option<PathBuf> {
    let first = args.first()?;
    if let Some(s) = first.as_str() {
        return Some(PathBuf::from(s));
    }
    if let Some(obj) = first.as_object() {
        if let Some(p) = obj.get("workingDirectory").and_then(|v| v.as_str()) {
            return Some(PathBuf::from(p));
        }
    }
    None
}

fn missing_arg(what: &str) -> jsonrpc::Error {
    jsonrpc::Error::invalid_params(format!("missing argument: {what}"))
}

async fn run_cli(
    backend: &Backend,
    dir: &std::path::Path,
    args: &[&str],
) -> jsonrpc::Result<Option<serde_json::Value>> {
    let cfg = backend.state.config.snapshot();
    if !cfg.cli_enabled {
        backend
            .client
            .log_message(
                MessageType::WARNING,
                "CLI commands are disabled via configuration",
            )
            .await;
        return Ok(Some(serde_json::json!({"skipped": "cli_disabled"})));
    }

    let mut cmd = Command::new(&cfg.cli_binary);
    cmd.args(args)
        .current_dir(dir)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = cmd.spawn().map_err(|e| {
        jsonrpc::Error::internal_error()
            .with_message(format!("failed to spawn {}: {e}", cfg.cli_binary))
    })?;

    let timeout = Duration::from_secs(cfg.cli_timeout.as_secs().max(1));
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(jsonrpc::Error::internal_error()
                .with_message(format!("{} failed to run: {e}", cfg.cli_binary)));
        }
        Err(_) => {
            return Err(jsonrpc::Error::internal_error().with_message(format!(
                "{} timed out after {}s",
                cfg.cli_binary,
                timeout.as_secs()
            )));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(-1);
    Ok(Some(serde_json::json!({
        "status": code,
        "stdout": stdout,
        "stderr": stderr,
    })))
}

trait JsonrpcErrorExt {
    fn with_message(self, msg: impl Into<String>) -> Self;
}

impl JsonrpcErrorExt for jsonrpc::Error {
    fn with_message(mut self, msg: impl Into<String>) -> Self {
        self.message = msg.into().into();
        self
    }
}
