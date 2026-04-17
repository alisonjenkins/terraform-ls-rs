//! Workspace discovery and file watching.

pub mod discovery;
pub mod error;
pub mod watcher;

pub use discovery::{discover_terraform_files, is_ignored_dir};
pub use error::WalkerError;
pub use watcher::{WorkspaceEvent, WorkspaceWatcher, watch_workspace};
