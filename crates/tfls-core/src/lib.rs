//! Core domain types for terraform-ls-rs.
//!
//! This crate defines the fundamental types used across the language server:
//! symbol tables, resource addresses, provider addresses, and module identifiers.

pub mod builtin_blocks;
pub mod completion;
pub mod error;
pub mod lock_file;
pub mod meta_arguments;
pub mod types;
pub mod uri;
pub mod variable_type;
pub mod version_constraint;

pub use completion::{
    classify_context, resolve_nested_schema, BlockStep, CompletionContext, IndexRootRef, PathStep,
};
pub use error::CoreError;
pub use meta_arguments::{
    condition_attr_description, content_meta_block_description, dynamic_meta_attr_description,
    is_meta_attr, is_singleton_meta_block, lifecycle_attr_description, lifecycle_attrs,
    lifecycle_block_description, lifecycle_blocks, meta_attr_description, meta_block_description,
    meta_blocks, BlockKind, CONDITION_ATTRS, META_ATTRS,
};
pub use types::{
    ModuleId, ProviderAddress, ResourceAddress, Symbol, SymbolKind, SymbolLocation, SymbolTable,
    SymbolVisitor,
};
pub use variable_type::{
    explain_mismatch, merge_shapes, parse_type_expr, parse_value_shape, satisfies, Primitive,
    VariableType,
};
