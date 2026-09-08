//! Small shared helpers for LSP handlers.
//!
//! The module-aggregation logic itself lives in `tfls-engine::module` now
//! (transport-free, shared with the future lint CLI); this re-export keeps
//! every existing `crate::handlers::util::X` call site resolving unchanged.

pub use tfls_engine::module::*;
