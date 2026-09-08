//! Module-scoped aggregation helpers shared by diagnostic and code-action
//! logic: resolving a document's module directory, walking sibling docs to
//! aggregate `required_version` / provider constraints, and resolving
//! `module { source = ... }` references to on-disk directories.

use std::path::PathBuf;

use url::Url;

/// Filesystem parent directory of a `file://` URI. Returns `None` for
/// URIs that can't be mapped to a path (e.g. exotic or non-file
/// schemes) so callers can degrade gracefully.
pub fn parent_dir(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()?.parent().map(|p| p.to_path_buf())
}
