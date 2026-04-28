//! Concurrent in-memory state store for terraform-ls-rs.
//!
//! Replaces HashiCorp's go-memdb with purpose-built `DashMap`-based
//! structures that provide fine-grained locking and zero-copy sharing
//! of immutable data (schemas) via `Arc`.

pub mod config;
pub mod document;
pub mod error;
pub mod index_cache;
pub mod jobs;
pub mod lookup;
pub mod store;

pub use config::{Config, ConfigCell, FormatStyle};
pub use document::DocumentState;
pub use error::StateError;
pub use index_cache::IndexCache;
pub use jobs::{Job, JobQueue, Priority};
pub use lookup::reference_at_position;
pub use store::{DirScanState, StateStore, SymbolKey, reference_key};
