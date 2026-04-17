use thiserror::Error;

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("terraform CLI execution failed")]
    CliExecution(#[source] std::io::Error),

    #[error("terraform CLI exited with status {status}: {stderr}")]
    CliFailed { status: i32, stderr: String },

    #[error("terraform CLI timed out after {timeout_secs}s")]
    CliTimeout { timeout_secs: u64 },

    #[error("failed to parse provider schema JSON")]
    JsonParse(#[source] sonic_rs::Error),

    #[error("schema not found for provider '{provider}'")]
    NotFound { provider: String },

    #[error("failed to decompress bundled schema '{name}'")]
    Decompression {
        name: String,
        #[source]
        source: std::io::Error,
    },

    #[error("schema cache I/O error")]
    Cache(#[source] std::io::Error),
}
