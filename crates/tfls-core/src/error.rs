use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid provider address '{input}': {reason}")]
    InvalidProviderAddress { input: String, reason: String },

    #[error("invalid resource address '{input}': {reason}")]
    InvalidResourceAddress { input: String, reason: String },
}
