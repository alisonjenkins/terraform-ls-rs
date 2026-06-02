//! Recursive walker over every [`Expression`] that appears inside a
//! [`Body`]. Diagnostic rules that need to inspect expression shapes
//! (deprecated operators, empty-list equality, duplicate object keys,
//! etc.) call [`for_each_expression`] with a visitor closure and
//! filter to the variants they care about.
//!
//! Walks into every compound form — arrays, objects, function-call
//! args, binary/unary operands, conditionals, for-expressions,
//! traversals, template interpolations, parenthesised sub-expressions
//! — so no expression position is missed.

use hcl_edit::expr::{Expression, ObjectKey};
use hcl_edit::structure::Body;
use hcl_edit::template::{Directive, Element};

/// Visit every expression in `body`, including those nested inside
/// block bodies, attributes, arrays, objects, function calls,
/// operations, conditionals, for-expressions, traversals, and
/// template interpolations.
pub fn for_each_expression<F>(body: &Body, mut visit: F)
where
    F: FnMut(&Expression),
{
    visit_body(body, &mut visit);
}

fn visit_body<F>(body: &Body, visit: &mut F)
where
    F: FnMut(&Expression),
{
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            visit_expr(&attr.value, visit);
        } else if let Some(block) = structure.as_block() {
            visit_body(&block.body, visit);
        }
    }
}

fn visit_expr<F>(expr: &Expression, visit: &mut F)
where
    F: FnMut(&Expression),
{
    // Fire on the current node first so outer-visible patterns
    // (e.g. `x == []`) see the whole BinaryOp before we recurse
    // into its operands.
    visit(expr);

    match expr {
        Expression::Null(_)
        | Expression::Bool(_)
        | Expression::Number(_)
        | Expression::String(_)
        | Expression::Variable(_) => {}
        Expression::Array(arr) => {
            for item in arr.iter() {
                visit_expr(item, visit);
            }
        }
        Expression::Object(obj) => {
            for (key, value) in obj.iter() {
                // Computed keys carry expressions too:
                // `{ (lookup(var.m, "k")) = 1 }`. Visit them so no
                // expression position is missed.
                if let ObjectKey::Expression(k) = key {
                    visit_expr(k, visit);
                }
                visit_expr(value.expr(), visit);
            }
        }
        Expression::StringTemplate(tpl) => visit_template_elements(tpl.iter(), visit),
        // Heredoc bodies (`<<-EOT ${...} EOT`) carry interpolations and
        // directives just like quoted templates. Recurse so no expression
        // position is missed (matches `tfls-parser::references`).
        Expression::HeredocTemplate(h) => visit_template_elements(h.template.iter(), visit),
        Expression::Parenthesis(p) => visit_expr(p.inner(), visit),
        Expression::Conditional(c) => {
            visit_expr(&c.cond_expr, visit);
            visit_expr(&c.true_expr, visit);
            visit_expr(&c.false_expr, visit);
        }
        Expression::FuncCall(call) => {
            for arg in call.args.iter() {
                visit_expr(arg, visit);
            }
        }
        Expression::Traversal(t) => {
            visit_expr(&t.expr, visit);
            // Traversal operators contain expressions too (e.g.
            // bracket indices): `foo[var.i]`.
            for op in t.operators.iter() {
                if let hcl_edit::expr::TraversalOperator::Index(idx) = op.value() {
                    visit_expr(idx, visit);
                }
            }
        }
        Expression::UnaryOp(op) => visit_expr(&op.expr, visit),
        Expression::BinaryOp(op) => {
            visit_expr(&op.lhs_expr, visit);
            visit_expr(&op.rhs_expr, visit);
        }
        Expression::ForExpr(f) => {
            visit_expr(&f.intro.collection_expr, visit);
            if let Some(key) = f.key_expr.as_ref() {
                visit_expr(key, visit);
            }
            visit_expr(&f.value_expr, visit);
            if let Some(cond) = f.cond.as_ref() {
                visit_expr(&cond.expr, visit);
            }
        }
    }
}

fn visit_template_elements<'a, I, F>(elements: I, visit: &mut F)
where
    I: IntoIterator<Item = &'a Element>,
    F: FnMut(&Expression),
{
    for element in elements {
        match element {
            Element::Literal(_) => {}
            Element::Interpolation(interp) => visit_expr(&interp.expr, visit),
            // `%{ if c }...%{ else }...%{ endif }` / `%{ for x in xs }...`
            // directives carry expressions too.
            Element::Directive(directive) => match directive.as_ref() {
                Directive::If(i) => {
                    visit_expr(&i.if_expr.cond_expr, visit);
                    visit_template_elements(i.if_expr.template.iter(), visit);
                    if let Some(else_part) = i.else_expr.as_ref() {
                        visit_template_elements(else_part.template.iter(), visit);
                    }
                }
                Directive::For(f) => {
                    visit_expr(&f.for_expr.collection_expr, visit);
                    visit_template_elements(f.for_expr.template.iter(), visit);
                }
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    /// Collect every `var.X` head ident the walker reaches.
    fn visited_vars(src: &str) -> Vec<String> {
        let body = parse_source(src).body.expect("parses");
        let mut names = Vec::new();
        for_each_expression(&body, |expr| {
            if let Expression::Traversal(t) = expr {
                if let Expression::Variable(v) = &t.expr {
                    names.push(v.as_str().to_string());
                }
            }
        });
        names
    }

    #[test]
    fn visits_computed_object_key_expression() {
        // `(var.k)` in key position is an expression that must be visited.
        let src = r#"output "x" { value = { (var.key) = 1 } }"#;
        assert!(
            visited_vars(src).contains(&"var".to_string()),
            "computed object key expression was not visited"
        );
    }

    #[test]
    fn visits_object_value_expression() {
        let src = r#"output "x" { value = { a = var.val } }"#;
        assert!(visited_vars(src).contains(&"var".to_string()));
    }

    #[test]
    fn recurses_into_heredoc_interpolation() {
        let src = "output \"x\" {\n  value = <<-EOT\n    ${var.greeting}\n  EOT\n}\n";
        assert!(
            visited_vars(src).contains(&"var".to_string()),
            "heredoc interpolation expression was not visited"
        );
    }

    #[test]
    fn recurses_into_heredoc_directive() {
        let src =
            "output \"x\" {\n  value = <<-EOT\n    %{ if var.on }${var.body}%{ endif }\n  EOT\n}\n";
        let vars = visited_vars(src);
        assert_eq!(vars.iter().filter(|v| *v == "var").count(), 2, "got: {vars:?}");
    }

    #[test]
    fn recurses_into_string_template_interpolation() {
        let src = "output \"x\" { value = \"pre-${var.mid}-post\" }";
        assert!(visited_vars(src).contains(&"var".to_string()));
    }
}
