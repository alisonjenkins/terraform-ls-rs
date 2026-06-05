//! `terraform_sensitive_output` — flag an `output` that exposes a
//! `sensitive = true` variable without marking itself `sensitive = true`.
//!
//! Terraform errors at plan time when a sensitive value flows into a
//! non-sensitive output ("Output refers to sensitive values"). The server
//! has the data to catch this in-editor: variables declared
//! `sensitive = true` and the output's value expression.
//!
//! Scope (v1): the SOURCE set is sensitive input *variables*. Schema-
//! sensitive resource attributes and `local` propagation are follow-ups.
//! A reference wrapped in `nonsensitive(...)` is intentional and not
//! flagged.

use std::collections::HashSet;

use hcl_edit::expr::{Expression, ObjectValue, TraversalOperator};
use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

/// Collect the names of `variable "X" { sensitive = true }` declarations
/// in `body`. Exposed so the LSP layer can aggregate across a module's
/// files before calling [`sensitive_output_diagnostics`].
pub fn sensitive_variable_names(body: &Body) -> HashSet<String> {
    let mut out = HashSet::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "variable" {
            continue;
        }
        let Some(name) = block.labels.first().and_then(label_str) else {
            continue;
        };
        if block_has_sensitive_true(block) {
            out.insert(name.to_string());
        }
    }
    out
}

/// Emit an ERROR for every `output` in `body` that references a sensitive
/// variable but is not itself `sensitive = true`. `extra_sensitive` carries
/// sensitive variable names declared in OTHER files of the same module
/// (variables and outputs commonly live in separate files); sensitive
/// variables declared in `body` itself are added automatically.
pub fn sensitive_output_diagnostics(
    body: &Body,
    rope: &Rope,
    extra_sensitive: &HashSet<String>,
) -> Vec<Diagnostic> {
    let mut sensitive = sensitive_variable_names(body);
    sensitive.extend(extra_sensitive.iter().cloned());
    if sensitive.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "output" {
            continue;
        }
        if block_has_sensitive_true(block) {
            continue; // correctly marked.
        }
        let Some(value) = output_value_expr(block) else {
            continue;
        };
        if !contains_sensitive_ref(value, &sensitive) {
            continue;
        }
        let Some(name_label) = block.labels.first() else {
            continue;
        };
        let Some(span) = name_label.span() else {
            continue;
        };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        let name = label_str(name_label).unwrap_or("");
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("terraform-ls-rs".to_string()),
            message: format!(
                "output `{name}` exposes a sensitive value — add `sensitive = true` \
                 (or wrap the value in `nonsensitive(...)` if intentional)"
            ),
            ..Default::default()
        });
    }
    out
}

fn output_value_expr(block: &Block) -> Option<&Expression> {
    block
        .body
        .iter()
        .filter_map(|s| s.as_attribute())
        .find(|a| a.key.as_str() == "value")
        .map(|a| &a.value)
}

/// `true` if `block` has `sensitive = true`.
fn block_has_sensitive_true(block: &Block) -> bool {
    block.body.iter().filter_map(|s| s.as_attribute()).any(|a| {
        a.key.as_str() == "sensitive" && matches!(&a.value, Expression::Bool(b) if *b.value())
    })
}

/// Whether `expr` references a sensitive `var.X` that is NOT inside a
/// `nonsensitive(...)` call. The walk stops descending into
/// `nonsensitive(...)` arguments — anything desensitized there is, by the
/// author's explicit intent, no longer sensitive.
fn contains_sensitive_ref(expr: &Expression, sensitive: &HashSet<String>) -> bool {
    match expr {
        Expression::Traversal(t) => {
            if let Some(name) = traversal_var_name(t) {
                if sensitive.contains(name) {
                    return true;
                }
            }
            // A traversal's base could itself be a compound expression.
            contains_sensitive_ref(&t.expr, sensitive)
        }
        Expression::FuncCall(call) => {
            if call.name.namespace.is_empty() && call.name.name.as_str() == "nonsensitive" {
                return false; // explicitly desensitized.
            }
            call.args.iter().any(|a| contains_sensitive_ref(a, sensitive))
        }
        Expression::Array(arr) => arr.iter().any(|e| contains_sensitive_ref(e, sensitive)),
        Expression::Object(obj) => obj.iter().any(|(k, v)| {
            let ObjectValue { .. } = v;
            let key_hit = matches!(k, hcl_edit::expr::ObjectKey::Expression(e) if contains_sensitive_ref(e, sensitive));
            key_hit || contains_sensitive_ref(v.expr(), sensitive)
        }),
        Expression::Parenthesis(p) => contains_sensitive_ref(p.inner(), sensitive),
        Expression::Conditional(c) => {
            contains_sensitive_ref(&c.cond_expr, sensitive)
                || contains_sensitive_ref(&c.true_expr, sensitive)
                || contains_sensitive_ref(&c.false_expr, sensitive)
        }
        Expression::UnaryOp(op) => contains_sensitive_ref(&op.expr, sensitive),
        Expression::BinaryOp(op) => {
            contains_sensitive_ref(&op.lhs_expr, sensitive)
                || contains_sensitive_ref(&op.rhs_expr, sensitive)
        }
        Expression::ForExpr(f) => {
            contains_sensitive_ref(&f.intro.collection_expr, sensitive)
                || f.key_expr
                    .as_ref()
                    .is_some_and(|k| contains_sensitive_ref(k, sensitive))
                || contains_sensitive_ref(&f.value_expr, sensitive)
                || f.cond
                    .as_ref()
                    .is_some_and(|c| contains_sensitive_ref(&c.expr, sensitive))
        }
        Expression::StringTemplate(tpl) => tpl.iter().any(|el| match el {
            hcl_edit::template::Element::Interpolation(i) => {
                contains_sensitive_ref(&i.expr, sensitive)
            }
            _ => false,
        }),
        _ => false,
    }
}

/// `Some("X")` for a `var.X[...]` traversal.
fn traversal_var_name(t: &hcl_edit::expr::Traversal) -> Option<&str> {
    let Expression::Variable(v) = &t.expr else {
        return None;
    };
    if v.value().as_str() != "var" {
        return None;
    }
    match t.operators.first().map(|o| o.value()) {
        Some(TraversalOperator::GetAttr(ident)) => Some(ident.as_str()),
        _ => None,
    }
}

fn label_str(label: &BlockLabel) -> Option<&str> {
    match label {
        BlockLabel::String(s) => Some(s.value().as_str()),
        BlockLabel::Ident(i) => Some(i.as_str()),
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
        sensitive_output_diagnostics(&body, &rope, &HashSet::new())
    }

    #[test]
    fn flags_sensitive_var_in_plain_output() {
        let d = diags("variable \"pw\" { sensitive = true }\noutput \"p\" { value = var.pw }\n");
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(d[0].message.contains("output `p`"), "got: {}", d[0].message);
    }

    #[test]
    fn silent_when_output_is_sensitive() {
        let d = diags(concat!(
            "variable \"pw\" { sensitive = true }\n",
            "output \"p\" {\n  value     = var.pw\n  sensitive = true\n}\n",
        ));
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_non_sensitive_var() {
        let d = diags("variable \"name\" {}\noutput \"n\" { value = var.name }\n");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_when_wrapped_in_nonsensitive() {
        let d = diags(
            "variable \"pw\" { sensitive = true }\noutput \"p\" { value = nonsensitive(var.pw) }\n",
        );
        assert!(d.is_empty(), "nonsensitive() opts out: {d:?}");
    }

    #[test]
    fn flags_sensitive_var_nested_in_expression() {
        let d = diags(
            "variable \"pw\" { sensitive = true }\noutput \"p\" { value = \"prefix-${var.pw}\" }\n",
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn flags_sensitive_var_in_interpolated_object() {
        let d = diags(
            "variable \"pw\" { sensitive = true }\noutput \"p\" { value = { secret = var.pw } }\n",
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn uses_external_sensitive_set() {
        // `pw` declared in a sibling file (passed via extra_sensitive).
        let rope = Rope::from_str("output \"p\" { value = var.pw }\n");
        let body = parse_source("output \"p\" { value = var.pw }\n")
            .body
            .expect("parses");
        let mut extra = HashSet::new();
        extra.insert("pw".to_string());
        let d = sensitive_output_diagnostics(&body, &rope, &extra);
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn collects_sensitive_variable_names() {
        let body = parse_source(
            "variable \"a\" { sensitive = true }\nvariable \"b\" {}\nvariable \"c\" { sensitive = true }\n",
        )
        .body
        .expect("parses");
        let names = sensitive_variable_names(&body);
        assert!(
            names.contains("a") && names.contains("c") && !names.contains("b"),
            "got: {names:?}"
        );
    }
}
