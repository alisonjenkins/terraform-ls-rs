//! `terraform_module_version` — flag `module "name" {}` blocks
//! whose `source` is a Terraform-registry path but that don't
//! declare a `version` constraint. Registry modules publish new
//! versions freely; without a pin the module can change under the
//! caller's feet.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn module_version_presence_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "module" {
            continue;
        }
        let mut source_literal: Option<String> = None;
        let mut has_version = false;
        let mut header_span = block.ident.span();
        if header_span.is_none() {
            header_span = Some(0..0);
        }
        for attr in block.body.iter().filter_map(|s| s.as_attribute()) {
            match attr.key.as_str() {
                "source" => {
                    if let Expression::String(s) = &attr.value {
                        source_literal = Some(s.value().as_str().to_string());
                    }
                }
                "version" => has_version = true,
                _ => {}
            }
        }
        let Some(source) = source_literal else {
            continue;
        };
        if has_version {
            continue;
        }
        if !looks_like_registry_source(&source) {
            continue;
        }
        let span = header_span.unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        let name = block
            .labels
            .first()
            .map(|l| match l {
                hcl_edit::structure::BlockLabel::String(s) => s.value().as_str().to_string(),
                hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
            })
            .unwrap_or_else(|| "?".to_string());
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: format!(
                "module `{name}` uses a registry source but has no `version` constraint"
            ),
            ..Default::default()
        });
    }
    out
}

/// Heuristic for "this module is pulled from a Terraform registry"
/// (public or private). Registry paths are of the form
/// `[host/]<ns>/<name>/<provider>` — three or four slash-separated
/// segments without a scheme, and not starting with `.`/`/`.
/// Git/hg/HTTP URLs, local paths, and S3/GCS sources are excluded
/// because they have their own pinning mechanics (git refs, etc.).
fn looks_like_registry_source(src: &str) -> bool {
    let trimmed = src.trim();
    // Non-registry sources all start with a scheme, protocol prefix,
    // or path indicator.
    if trimmed.is_empty()
        || trimmed.starts_with('.')
        || trimmed.starts_with('/')
        || trimmed.starts_with("git::")
        || trimmed.starts_with("hg::")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("s3::")
        || trimmed.starts_with("gcs::")
        || trimmed.starts_with("github.com/")
        || trimmed.starts_with("bitbucket.org/")
    {
        return false;
    }
    let segments: Vec<&str> = trimmed.split('/').collect();
    segments.len() == 3 || segments.len() == 4
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        module_version_presence_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_registry_module_without_version() {
        let d = diags(
            r#"module "vpc" { source = "terraform-aws-modules/vpc/aws" }"#,
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("`vpc`"), "got: {}", d[0].message);
    }

    #[test]
    fn silent_when_version_set() {
        let d = diags(
            r#"module "vpc" {
                source  = "terraform-aws-modules/vpc/aws"
                version = "~> 5.0"
            }"#,
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_local_source() {
        let d = diags(r#"module "x" { source = "./modules/x" }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_git_source() {
        let d = diags(
            r#"module "x" { source = "git::https://example.com/foo.git?ref=v1.0" }"#,
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_private_registry_source() {
        let d = diags(
            r#"module "x" { source = "app.terraform.io/example-corp/vpc/aws" }"#,
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
    }
}
