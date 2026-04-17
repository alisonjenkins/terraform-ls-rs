//! Core domain types for terraform-ls-rs.
//!
//! This crate defines the fundamental types used across the language server:
//! symbol tables, resource addresses, provider addresses, and module identifiers.

pub mod completion;
pub mod error;
pub mod types;

pub use completion::{CompletionContext, classify_context};
pub use error::CoreError;
pub use types::{
    ModuleId, ProviderAddress, ResourceAddress, Symbol, SymbolKind, SymbolLocation, SymbolTable,
};
