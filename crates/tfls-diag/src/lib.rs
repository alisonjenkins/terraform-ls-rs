//! Diagnostics engine for terraform-ls-rs.

pub mod error;
pub mod references;
pub mod schema_validation;
pub mod syntax;

pub use error::DiagError;
pub use references::{undefined_reference_diagnostics, undefined_reference_diagnostics_for_document};
pub use schema_validation::resource_diagnostics;
pub use syntax::diagnostics_for_parse_errors;
