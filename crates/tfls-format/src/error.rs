use thiserror::Error;

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("failed to parse HCL for formatting")]
    Parse(#[source] hcl_edit::parser::Error),
}
