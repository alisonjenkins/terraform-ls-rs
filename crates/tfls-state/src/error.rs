use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("document not found: {uri}")]
    DocumentNotFound { uri: String },

    #[error("failed to apply text change to document '{uri}'")]
    EditApplication {
        uri: String,
        #[source]
        source: tfls_parser::ParseError,
    },
}
