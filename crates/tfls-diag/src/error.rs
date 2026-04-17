use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiagError {
    #[error("failed to compute diagnostics")]
    Compute(#[source] tfls_parser::ParseError),
}
