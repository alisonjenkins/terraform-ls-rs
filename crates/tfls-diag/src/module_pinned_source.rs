//! `terraform_module_pinned_source` — flag git/hg `module.source`
//! URLs that don't pin a ref via `?ref=…` or `?rev=…`. Without a
//! pin, `terraform init` just grabs the default branch, and running
//! the same code a week later can yield a different module.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn module_pinned_source_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
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
            let kind = classify_source(raw);
            if !matches!(kind, SourceKind::Git | SourceKind::Hg) {
                continue;
            }
            if has_pinned_ref(raw, kind) {
                continue;
            }
            let span = attr.span().unwrap_or(0..0);
            let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
            let kind_word = match kind {
                SourceKind::Git => "git",
                SourceKind::Hg => "mercurial",
                _ => unreachable!(),
            };
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message: format!(
                    "module source is a {kind_word} URL but has no pinned revision (add `?ref=…`)"
                ),
                ..Default::default()
            });
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Git,
    Hg,
    Other,
}

fn classify_source(src: &str) -> SourceKind {
    let trimmed = src.trim();
    if trimmed.starts_with("git::")
        || trimmed.starts_with("github.com/")
        || trimmed.starts_with("bitbucket.org/")
        || trimmed.ends_with(".git")
        || trimmed.contains(".git?")
        || trimmed.contains(".git#")
    {
        SourceKind::Git
    } else if trimmed.starts_with("hg::") {
        SourceKind::Hg
    } else {
        SourceKind::Other
    }
}

fn has_pinned_ref(src: &str, kind: SourceKind) -> bool {
    match kind {
        SourceKind::Git => {
            // `?ref=...` is the canonical pin for git; `?rev=...` for hg.
            // Also accept `#<sha>` URL fragments (less common but used
            // for GitHub) — tflint accepts them.
            src.contains("?ref=")
                || src.contains("&ref=")
                || src.contains("#")
        }
        SourceKind::Hg => src.contains("?rev=") || src.contains("&rev="),
        SourceKind::Other => true,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        module_pinned_source_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_unpinned_git_source() {
        let d = diags(
            r#"module "x" { source = "git::https://example.com/foo.git" }"#,
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("git"), "got: {}", d[0].message);
        assert!(d[0].message.contains("ref="), "got: {}", d[0].message);
    }

    #[test]
    fn silent_when_git_source_pinned_with_ref() {
        let d = diags(
            r#"module "x" { source = "git::https://example.com/foo.git?ref=v1.0.0" }"#,
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_when_git_source_pinned_with_fragment() {
        let d = diags(
            r#"module "x" { source = "git::https://example.com/foo.git#abc123" }"#,
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_unpinned_github_shorthand() {
        let d = diags(
            r#"module "x" { source = "github.com/example/foo" }"#,
        );
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn silent_for_registry_and_local_sources() {
        let d = diags(r#"module "x" { source = "./modules/x" }"#);
        assert!(d.is_empty(), "got: {d:?}");
        let d = diags(r#"module "x" { source = "hashicorp/consul/aws" }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_unpinned_mercurial_source() {
        let d = diags(r#"module "x" { source = "hg::https://example.com/foo" }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }
}
