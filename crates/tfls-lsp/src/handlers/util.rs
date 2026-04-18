//! Small shared helpers for LSP handlers.

use std::path::PathBuf;

use lsp_types::Url;

/// Filesystem parent directory of a `file://` URI. Returns `None` for
/// URIs that can't be mapped to a path (e.g. exotic or non-file
/// schemes) so callers can degrade gracefully.
pub(crate) fn parent_dir(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()?.parent().map(|p| p.to_path_buf())
}
