//! `terraform_module_mutable_ref` — flag a git `module.source` whose `?ref=`
//! (or `#fragment`) pins a MUTABLE ref (a tag or branch) rather than an
//! immutable commit SHA. A re-pointed tag can swap in poisoned code; a SHA
//! can't. Pairs with a code action that resolves the ref → its commit SHA and
//! keeps the tag as a trailing comment.
//!
//! Pure / offline: only the string is inspected. The tag→SHA lookup happens
//! lazily in the code action.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_core::git_ref::looks_like_commit_sha;
use tfls_parser::hcl_span_to_lsp_range;

use crate::git_source::{extract_ref, is_git_source};

pub fn module_mutable_ref_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
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
            let Some(r) = extract_ref(raw) else {
                continue; // no ref → module_pinned_source's concern
            };
            if looks_like_commit_sha(r) {
                continue; // already immutable
            }
            // Range the quoted value (NOT attr.span()): the code action needs
            // this exact range to find the attribute and compute offsets.
            let Some(span) = attr.value.span() else {
                continue;
            };
            let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message: format!(
                    "module source is pinned to mutable git ref `{r}`; pin to an immutable commit SHA"
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

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        module_mutable_ref_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_version_tag() {
        let d = diags(r#"module "x" { source = "git::ssh://git@h/o/r?ref=v1.2.3" }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("mutable git ref `v1.2.3`"));
    }

    #[test]
    fn flags_branch() {
        let d = diags(r#"module "x" { source = "git::ssh://git@h/o/r?ref=main" }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn flags_short_hex_below_seven() {
        let d = diags(r#"module "x" { source = "git::ssh://git@h/o/r?ref=abc12" }"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn silent_for_full_sha() {
        let sha = "9f8e7d6c5b4a3f2e1d0c9b8a7f6e5d4c3b2a1f0e";
        let d = diags(&format!(
            r#"module "x" {{ source = "git::ssh://git@h/o/r?ref={sha}" }}"#
        ));
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_abbrev_sha_seven_plus() {
        let d = diags(r#"module "x" { source = "git::ssh://git@h/o/r?ref=abc1234" }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_no_ref() {
        let d = diags(r#"module "x" { source = "git::ssh://git@h/o/r" }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_local_and_registry() {
        assert!(diags(r#"module "x" { source = "./local" }"#).is_empty());
        assert!(diags(r#"module "x" { source = "hashicorp/consul/aws" }"#).is_empty());
    }

    #[test]
    fn multiple_modules() {
        let src = concat!(
            "module \"a\" { source = \"git::ssh://git@h/o/r?ref=v1.0.0\" }\n",
            "module \"b\" { source = \"git::ssh://git@h/o/r?ref=v2.0.0\" }\n",
        );
        assert_eq!(diags(src).len(), 2);
    }
}
