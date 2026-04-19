//! HCL parsing layer for terraform-ls-rs.
//!
//! Wraps `hcl-edit` to provide parsing, position conversion between LSP
//! and byte offsets via `ropey`, and incremental parsing support.

pub mod error;
pub mod fallback_symbols;
pub mod json;
pub mod parse;
pub mod position;
pub mod references;
pub mod traversal;

pub use error::ParseError;
pub use fallback_symbols::extract_symbols_fallback;
pub use json::parse_json_source;
pub use parse::{ParsedFile, parse_source, parse_source_for_uri};
pub use position::{
    byte_offset_to_lsp_position, hcl_span_to_lsp_range, lsp_position_to_byte_offset,
};
pub use references::{Reference, ReferenceKind, extract_references};
pub use traversal::extract_symbols;
