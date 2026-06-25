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

/// Whether `uri_or_path` names a Terraform/OpenTofu TEST file
/// (`.tftest.hcl` / `.tftest.json` / `.tofutest.hcl` / `.tofutest.json`).
///
/// Test files are parsed like HCL/JSON but are NOT module configuration:
/// their `run`/`variables`/`provider` blocks must not contribute symbols to
/// the module index, the module-only diagnostics don't apply, and their
/// references resolve against the module UNDER TEST, not the test file's own
/// directory. The parser, walker, diagnostics and completion all branch on
/// this.
pub fn is_tftest_uri(uri_or_path: &str) -> bool {
    uri_or_path.ends_with(".tftest.hcl")
        || uri_or_path.ends_with(".tftest.json")
        || uri_or_path.ends_with(".tofutest.hcl")
        || uri_or_path.ends_with(".tofutest.json")
}

/// Whether `uri_or_path` is a JSON-syntax test file (`.tftest.json` /
/// `.tofutest.json`) — routed to the JSON parser, like `.tf.json`.
pub fn is_tftest_json(uri_or_path: &str) -> bool {
    uri_or_path.ends_with(".tftest.json") || uri_or_path.ends_with(".tofutest.json")
}

#[cfg(test)]
mod tftest_kind_tests {
    use super::{is_tftest_json, is_tftest_uri};

    #[test]
    fn recognizes_test_files() {
        for p in [
            "file:///m/a.tftest.hcl",
            "file:///m/tests/a.tftest.json",
            "/m/a.tofutest.hcl",
            "/m/a.tofutest.json",
        ] {
            assert!(is_tftest_uri(p), "{p} should be a test file");
        }
        for p in [
            "file:///m/main.tf",
            "/m/x.tfvars",
            "/m/a.tf.json",
            "/m/a.hcl",
        ] {
            assert!(!is_tftest_uri(p), "{p} should NOT be a test file");
        }
    }

    #[test]
    fn json_variant_detection() {
        assert!(is_tftest_json("/m/a.tftest.json"));
        assert!(is_tftest_json("/m/a.tofutest.json"));
        assert!(!is_tftest_json("/m/a.tftest.hcl"));
        assert!(!is_tftest_json("/m/a.tf.json"));
    }
}
