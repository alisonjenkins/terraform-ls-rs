//! `terraform_module_outdated` — INFORMATION diagnostic when a git module's
//! pinned version has a newer tag available in the SAME namespace (handles
//! monorepo per-module tags like `modules/vpc/v1.2.3`).
//!
//! The module's "current version" is the `?ref=<tag>` value when that ref is a
//! tag, or the `# <tag>` comment when the ref is a commit SHA. Offline: the tag
//! list is injected as a closure reading the prefetch-populated cache; a cold
//! cache yields nothing.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_core::git_ref::{looks_like_commit_sha, newer_versions};
use tfls_parser::hcl_span_to_lsp_range;

use crate::git_source::{extract_ref, is_git_source, trailing_comment_tag};

/// `cached_tags(source)` returns every tag name in the module's repo, from
/// cache, or `None` if not cached.
pub fn module_outdated_diagnostics(
    body: &Body,
    rope: &Rope,
    cached_tags: &dyn Fn(&str) -> Option<Vec<String>>,
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
            let Some(span) = attr.value.span() else {
                continue;
            };
            // Current version tag: the ref if it's a tag, else the comment.
            let current = match extract_ref(raw) {
                Some(r) if !looks_like_commit_sha(r) => r.to_string(),
                Some(_) => match trailing_comment_tag(rope, span.end) {
                    Some(t) => t,
                    None => continue,
                },
                None => continue,
            };
            let Some(tags) = cached_tags(raw) else {
                continue;
            };
            let newer = newer_versions(&tags, &current);
            let Some(latest) = newer.first() else {
                continue;
            };
            let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::INFORMATION),
                source: Some("terraform-ls-rs".to_string()),
                message: format!(
                    "newer module version available: `{latest}` (current `{current}`)"
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

    fn diags(src: &str, tags: &dyn Fn(&str) -> Option<Vec<String>>) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        module_outdated_diagnostics(&body, &rope, tags)
    }

    #[test]
    fn flags_newer_tag_ref() {
        let src = r#"module "x" { source = "git::ssh://git@h/o/r?ref=v1.0.0" }"#;
        let d = diags(src, &|_s| Some(vec!["v1.0.0".into(), "v1.1.0".into()]));
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("v1.1.0"));
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::INFORMATION));
    }

    #[test]
    fn flags_newer_for_sha_with_comment() {
        let sha = "1111111111111111111111111111111111111111";
        let src = format!(
            "module \"x\" {{\n  source = \"git::ssh://git@h/o/r?ref={sha}\" # v1.0.0\n}}\n"
        );
        let d = diags(&src, &|_s| Some(vec!["v1.0.0".into(), "v2.0.0".into()]));
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("v2.0.0"));
    }

    #[test]
    fn monorepo_other_prefix_ignored() {
        let src =
            r#"module "x" { source = "git::ssh://git@h/o/r//modules/vpc?ref=modules/vpc/v1.0.0" }"#;
        let d = diags(src, &|_s| {
            Some(vec![
                "modules/vpc/v1.0.0".into(),
                "modules/rds/v9.9.9".into(),
            ])
        });
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_when_latest() {
        let src = r#"module "x" { source = "git::ssh://git@h/o/r?ref=v2.0.0" }"#;
        let d = diags(src, &|_s| Some(vec!["v1.0.0".into(), "v2.0.0".into()]));
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_on_cold_cache() {
        let src = r#"module "x" { source = "git::ssh://git@h/o/r?ref=v1.0.0" }"#;
        assert!(diags(src, &|_s| None).is_empty());
    }
}
