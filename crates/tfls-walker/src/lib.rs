//! Workspace discovery and file watching.

pub mod discovery;
pub mod error;
pub mod watcher;

pub use discovery::{
    discover_terraform_files, discover_terraform_files_in_dir, discover_tfvars_attributable_to,
    discover_tfvars_files, discover_tfvars_files_in_dir, is_ignored_dir, is_terraform_file,
    is_tfvars_file,
};
pub use error::WalkerError;
pub use watcher::{WorkspaceEvent, WorkspaceWatcher, watch_workspace};
