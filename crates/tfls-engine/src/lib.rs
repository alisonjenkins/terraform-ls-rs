//! Transport-free diagnostics engine shared by the LSP server and the
//! (future) standalone lint CLI.

pub mod format_scan;
pub mod index;
pub mod module;
pub mod pipeline;
pub mod provider_fn;
pub mod snapshot;
