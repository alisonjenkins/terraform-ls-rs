//! Core domain types for terraform-ls-rs.
//!
//! This crate defines the fundamental types used across the language server:
//! symbol tables, resource addresses, provider addresses, and module identifiers.

pub mod completion;
pub mod error;
pub mod meta_arguments;
pub mod types;

pub use completion::{CompletionContext, classify_context};
pub use error::CoreError;
pub use meta_arguments::{
    BlockKind, CONDITION_ATTRS, META_ATTRS, is_meta_attr, is_singleton_meta_block,
    lifecycle_attrs, lifecycle_blocks, meta_blocks,
};
pub use types::{
    ModuleId, ProviderAddress, ResourceAddress, Symbol, SymbolKind, SymbolLocation, SymbolTable,
};
