//! `terraform_workspace_remote` — flag `terraform.workspace`
//! references when the backend is HCP Terraform / remote. In those
//! backends the concept of "workspace" maps to a remote workspace
//! object, not a CLI-local namespace, so branching on
//! `terraform.workspace` produces surprising results.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::expr_walk::for_each_expression;

pub fn workspace_remote_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    if !uses_remote_backend(body) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for_each_expression(body, |expr| {
        if is_terraform_workspace(expr) {
            let span = expr.span().unwrap_or(0..0);
            let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message:
                    "`terraform.workspace` is not meaningful with `cloud {}` or `backend \"remote\"`"
                        .to_string(),
                ..Default::default()
            });
        }
    });
    out
}

fn uses_remote_backend(body: &Body) -> bool {
    for structure in body.iter() {
        let Some(tf_block) = structure.as_block() else {
            continue;
        };
        if tf_block.ident.as_str() != "terraform" {
            continue;
        }
        for inner in tf_block.body.iter() {
            let Some(block) = inner.as_block() else {
                continue;
            };
            match block.ident.as_str() {
                "cloud" => return true,
                "backend" => {
                    if let Some(label) = block.labels.first() {
                        let name = match label {
                            hcl_edit::structure::BlockLabel::String(s) => {
                                s.value().as_str().to_string()
                            }
                            hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
                        };
                        if name == "remote" {
                            return true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    false
}

fn is_terraform_workspace(expr: &Expression) -> bool {
    let Expression::Traversal(t) = expr else {
        return false;
    };
    // Root must be the bare `terraform` identifier.
    let Expression::Variable(root) = &t.expr else {
        return false;
    };
    if root.value().as_str() != "terraform" {
        return false;
    }
    // First operator must be `.workspace`.
    let first_op = t.operators.first();
    let Some(op) = first_op else { return false };
    if let hcl_edit::expr::TraversalOperator::GetAttr(name) = op.value() {
        return name.value().as_str() == "workspace";
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        workspace_remote_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_workspace_under_cloud_backend() {
        let d = diags(
            r#"terraform {
                cloud {
                    organization = "acme"
                }
            }
            output "x" { value = terraform.workspace }"#,
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn flags_workspace_under_remote_backend() {
        let d = diags(
            r#"terraform {
                backend "remote" {
                    organization = "acme"
                }
            }
            output "x" { value = terraform.workspace }"#,
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn silent_for_local_backend() {
        let d = diags(
            r#"terraform {
                backend "local" {}
            }
            output "x" { value = terraform.workspace }"#,
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_without_backend() {
        let d = diags(r#"output "x" { value = terraform.workspace }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }
}
