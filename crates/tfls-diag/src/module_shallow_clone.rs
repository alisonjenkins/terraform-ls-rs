//! `terraform_module_shallow_clone` — when a git module source is
//! pinned to a specific tag (e.g. `?ref=v1.2.3`), advise adding
//! `depth=1` so `terraform init` only fetches that ref's commit
//! instead of the whole history. Saves time and bandwidth on CI.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn module_shallow_clone_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
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
            if !is_pinned_to_tag(raw) {
                continue;
            }
            if has_depth_one(raw) {
                continue;
            }
            let span = attr.span().unwrap_or(0..0);
            let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message:
                    "pinned git module source should add `depth=1` for a shallow clone"
                        .to_string(),
                ..Default::default()
            });
        }
    }
    out
}

fn is_git_source(src: &str) -> bool {
    let trimmed = src.trim();
    trimmed.starts_with("git::")
        || trimmed.starts_with("github.com/")
        || trimmed.starts_with("bitbucket.org/")
        || trimmed.ends_with(".git")
        || trimmed.contains(".git?")
        || trimmed.contains(".git#")
}

/// Heuristic for "pinned to a tag, not a branch" — `ref=vX.Y.Z`,
/// `ref=X.Y.Z`, or a commit SHA. Skip when pinned to something that
/// looks like a branch name (`main`, `master`, arbitrary word). Only
/// recommend `depth=1` for tag/sha pins because shallow-cloning a
/// branch defeats `terraform init`'s ability to re-resolve later.
fn is_pinned_to_tag(src: &str) -> bool {
    let Some(ref_val) = extract_ref(src) else {
        return false;
    };
    // Tag-ish: starts with v and a digit, or is all-digits-and-dots,
    // or is a 7+ char hex sha.
    let first = ref_val.chars().next().unwrap_or(' ');
    if (first == 'v' || first == 'V') && ref_val.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
        return true;
    }
    if ref_val.chars().all(|c| c.is_ascii_digit() || c == '.') && !ref_val.is_empty() {
        return true;
    }
    if ref_val.len() >= 7 && ref_val.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    false
}

fn extract_ref(src: &str) -> Option<&str> {
    for marker in ["?ref=", "&ref="] {
        if let Some(start) = src.find(marker) {
            let rest = &src[start + marker.len()..];
            let end = rest.find('&').unwrap_or(rest.len());
            return Some(&rest[..end]);
        }
    }
    if let Some(start) = src.find('#') {
        return Some(&src[start + 1..]);
    }
    None
}

fn has_depth_one(src: &str) -> bool {
    src.contains("depth=1") || src.contains("depth%3D1")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        module_shallow_clone_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_tag_pinned_git_source_without_depth() {
        let d = diags(
            r#"module "x" { source = "git::https://example.com/foo.git?ref=v1.2.3" }"#,
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("depth=1"));
    }

    #[test]
    fn silent_when_depth_one_set() {
        let d = diags(
            r#"module "x" { source = "git::https://example.com/foo.git?ref=v1.2.3&depth=1" }"#,
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_branch_pin() {
        let d = diags(
            r#"module "x" { source = "git::https://example.com/foo.git?ref=main" }"#,
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_unpinned_source() {
        // Separate rule (module_pinned_source) handles the unpinned
        // case; we only talk about shallow-clone optimisation when a
        // pin already exists.
        let d = diags(r#"module "x" { source = "git::https://example.com/foo.git" }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_commit_sha_pin_without_depth() {
        let d = diags(
            r#"module "x" { source = "git::https://example.com/foo.git?ref=abc1234" }"#,
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
    }
}
