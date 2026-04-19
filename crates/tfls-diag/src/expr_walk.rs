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

use hcl_edit::expr::{Expression, ObjectValue};
use hcl_edit::structure::Body;
use hcl_edit::template::{Element, StringTemplate};

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
        | Expression::Variable(_)
        | Expression::HeredocTemplate(_) => {}
        Expression::Array(arr) => {
            for item in arr.iter() {
                visit_expr(item, visit);
            }
        }
        Expression::Object(obj) => {
            for (_key, value) in obj.iter() {
                match value {
                    ObjectValue { .. } => visit_expr(value.expr(), visit),
                }
            }
        }
        Expression::StringTemplate(tpl) => visit_template(tpl, visit),
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

fn visit_template<F>(tpl: &StringTemplate, visit: &mut F)
where
    F: FnMut(&Expression),
{
    for element in tpl.iter() {
        match element {
            Element::Interpolation(interp) => visit_expr(&interp.expr, visit),
            Element::Directive(_) | Element::Literal(_) => {}
        }
    }
}
