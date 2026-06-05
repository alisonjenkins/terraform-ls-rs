//! Conversions between our internal [`url::Url`] document identifiers and the
//! LSP [`lsp_types::Uri`] type.
//!
//! ls-types (the lsp-types fork used by tower-lsp-server) models document URIs
//! as an opaque `Uri` newtype over `fluent_uri::Uri<String>` with no file-path
//! helpers, so internally we keep `url::Url` (which has `from_file_path` /
//! `to_file_path` / scheme accessors) and convert only at the LSP boundary.

use std::str::FromStr;

use url::Url;

/// Convert an internal [`Url`] into an LSP [`Uri`](lsp_types::Uri).
///
/// `url::Url` always renders to a valid absolute URI string and ls-types' `Uri`
/// accepts any RFC 3986 URI, so the parse is total: an `Err` here would mean the
/// `url` crate produced a non-URI string, which it never does.
#[expect(
    clippy::expect_used,
    reason = "url::Url always renders a valid absolute URI; the parse is total"
)]
pub fn url_to_uri(url: &Url) -> lsp_types::Uri {
    lsp_types::Uri::from_str(url.as_str()).expect("url::Url renders a valid URI")
}

/// Convert a `WorkspaceEdit`-style `changes` map keyed by internal [`Url`] into
/// one keyed by LSP [`Uri`](lsp_types::Uri), for the LSP boundary.
pub fn changes_to_uri<S: std::hash::BuildHasher + Default>(
    changes: std::collections::HashMap<Url, Vec<lsp_types::TextEdit>, S>,
) -> std::collections::HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>> {
    changes
        .into_iter()
        .map(|(url, edits)| (url_to_uri(&url), edits))
        .collect()
}

/// Parse an LSP [`Uri`](lsp_types::Uri) back into an internal [`Url`].
///
/// Returns `None` for URIs the `url` crate can't represent (e.g. relative
/// references), which callers at the LSP boundary surface as an error.
pub fn uri_to_url(uri: &lsp_types::Uri) -> Option<Url> {
    Url::parse(uri.as_str()).ok()
}
