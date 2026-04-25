use thiserror::Error;

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("failed to parse HCL for formatting")]
    Parse(#[source] hcl_edit::parser::Error),

    /// hcl-edit's parser hit one of its internal `.unwrap()` /
    /// `panic!` sites — see `tfls_parser::safe` for the upstream
    /// audit. We refuse to format inputs that crashed the parser.
    #[error("HCL parser panicked while validating: {0}")]
    Panicked(#[from] tfls_parser::ParsePanic),
}
