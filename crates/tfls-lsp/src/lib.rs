//! `tower-lsp` backend for terraform-ls-rs.

pub mod backend;
pub mod capabilities;
pub mod error;
pub mod handlers;
pub mod indexer;
pub mod progress;

pub use backend::Backend;
pub use error::LspError;
