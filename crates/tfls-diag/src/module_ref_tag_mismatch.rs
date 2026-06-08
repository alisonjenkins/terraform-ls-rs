//! `terraform_module_ref_tag_mismatch` — when a git `module.source` pins a
//! commit SHA AND carries a `# <tag>` comment, verify the tag still resolves to
//! that commit. If the cached `git ls-remote` says the tag now points elsewhere,
//! the tag was re-pointed upstream (the poisoning threat) or the SHA/comment
//! were edited inconsistently. WARNING.
//!
//! Offline: the resolved-SHA lookup is injected as a closure that reads the
//! on-disk cache the prefetch job populates. A cold cache yields no diagnostic
//! (never a false positive). Keeping the lookup behind a closure keeps this
//! crate free of a `tfls-provider-protocol` dependency and makes the rule
//! unit-testable with a stub map.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_core::git_ref::{looks_like_commit_sha, sha_matches};
use tfls_parser::hcl_span_to_lsp_range;

use crate::git_source::{extract_ref, is_git_source, trailing_comment_tag};

/// `cached(source, tag)` returns the commit SHA the tag currently resolves to,
/// from cache, or `None` if not cached / unknown.
pub fn module_ref_tag_mismatch_diagnostics(
    body: &Body,
    rope: &Rope,
    cached: &dyn Fn(&str, &str) -> Option<String>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "module" {
            continue;
        }
        for attr in block.body.iter().filter_map(|s| s.as_attribute()) {
            if attr.key.as_str() != "source" {
                continue;
            }
            let Expression::String(s) = &attr.value else {
                continue;
            };
            let raw = s.value().as_str();
            if !is_git_source(raw) {
                continue;
            }
            let Some(pinned) = extract_ref(raw) else {
                continue;
            };
            if !looks_like_commit_sha(pinned) {
                continue; // only SHA pins can mismatch a tag comment
            }
            let Some(span) = attr.value.span() else {
                continue;
            };
            let Some(tag) = trailing_comment_tag(rope, span.end) else {
                continue; // no comment to verify
            };
            let Some(resolved) = cached(raw, &tag) else {
                continue; // cold cache → say nothing
            };
            if sha_matches(&resolved, pinned) {
                continue; // consistent
            }
            let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message: format!(
                    "pinned commit `{pinned}` does not match tag `{tag}` (now `{resolved}`) — the tag may have been re-pointed"
                ),
                ..Default::default()
            });
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    const SHA1: &str = "1111111111111111111111111111111111111111";
    const SHA2: &str = "2222222222222222222222222222222222222222";

    fn diags(src: &str, cached: &dyn Fn(&str, &str) -> Option<String>) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        module_ref_tag_mismatch_diagnostics(&body, &rope, cached)
    }

    #[test]
    fn flags_mismatch() {
        let src = format!(
            "module \"x\" {{\n  source = \"git::ssh://git@h/o/r?ref={SHA1}\" # v1.2.3\n}}\n"
        );
        let d = diags(&src, &|_s, _t| Some(SHA2.to_string()));
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("does not match tag"));
    }

    #[test]
    fn silent_when_consistent() {
        let src = format!(
            "module \"x\" {{\n  source = \"git::ssh://git@h/o/r?ref={SHA1}\" # v1.2.3\n}}\n"
        );
        let d = diags(&src, &|_s, _t| Some(SHA1.to_string()));
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_on_cold_cache() {
        let src = format!(
            "module \"x\" {{\n  source = \"git::ssh://git@h/o/r?ref={SHA1}\" # v1.2.3\n}}\n"
        );
        let d = diags(&src, &|_s, _t| None);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn abbrev_sha_prefix_match_consistent() {
        let src = "module \"x\" {\n  source = \"git::ssh://git@h/o/r?ref=1111111\" # v1.2.3\n}\n";
        let d = diags(src, &|_s, _t| Some(SHA1.to_string()));
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_when_no_comment() {
        let src = format!("module \"x\" {{\n  source = \"git::ssh://git@h/o/r?ref={SHA1}\"\n}}\n");
        let d = diags(&src, &|_s, _t| Some(SHA2.to_string()));
        assert!(d.is_empty(), "got: {d:?}");
    }
}
