//! `terraform_cyclic_locals` — detect dependency cycles among `local`
//! values within a file.
//!
//! Terraform errors on a cycle (`Cycle: local.a, local.b`); a self- or
//! mutually-referential local is otherwise invisible until plan time.
//! This catches same-file cycles from the locals' reference edges. (A
//! cycle spanning `locals` blocks in *different* files of one module is a
//! follow-up; same-file covers the overwhelming majority.)

use std::collections::{HashMap, HashSet};

use hcl_edit::expr::{Expression, TraversalOperator};
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use hcl_edit::template::Element;
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

struct LocalDef {
    deps: HashSet<String>,
    range: Range,
}

pub fn cyclic_locals_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let defs = collect_locals(body, rope);
    if defs.is_empty() {
        return Vec::new();
    }

    // Find every local that participates in a cycle (including a
    // self-reference). A node is on a cycle iff it can reach itself
    // through the dependency edges.
    let mut out = Vec::new();
    let mut reported: HashSet<String> = HashSet::new();
    for name in defs.keys() {
        if reported.contains(name) {
            continue;
        }
        if let Some(path) = find_cycle_from(name, &defs) {
            // Report each member of the cycle once, with the readable path.
            let rendered = render_path(&path);
            for member in &path {
                if !reported.insert(member.clone()) {
                    continue;
                }
                if let Some(def) = defs.get(member) {
                    out.push(Diagnostic {
                        range: def.range,
                        severity: Some(DiagnosticSeverity::ERROR),
                        source: Some("terraform-ls-rs".to_string()),
                        message: format!("`local.{member}` is part of a dependency cycle: {rendered}"),
                        ..Default::default()
                    });
                }
            }
        }
    }
    out.sort_by_key(|d| (d.range.start.line, d.range.start.character));
    out
}

/// Collect every `local` name → (referenced locals, key range) across all
/// `locals { … }` blocks in `body`.
fn collect_locals(body: &Body, rope: &Rope) -> HashMap<String, LocalDef> {
    let mut defs = HashMap::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "locals" {
            continue;
        }
        for entry in block.body.iter() {
            let Some(attr) = entry.as_attribute() else {
                continue;
            };
            let name = attr.key.as_str().to_string();
            let Some(span) = attr.key.span() else { continue };
            let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
                continue;
            };
            let mut deps = HashSet::new();
            visit_expr(&attr.value, &mut |expr| {
                if let Some(local) = referenced_local(expr) {
                    deps.insert(local.to_string());
                }
            });
            // Later definitions of the same name win the range, but the
            // dependency set is the union (Terraform errors on duplicate
            // locals anyway — see duplicate_definition).
            let dep_union = defs
                .remove(&name)
                .map(|d: LocalDef| {
                    let mut u = d.deps;
                    u.extend(deps.iter().cloned());
                    u
                })
                .unwrap_or(deps);
            defs.insert(name, LocalDef { deps: dep_union, range });
        }
    }
    defs
}

/// Walk an expression and its sub-expressions, calling `visit` on each.
fn visit_expr<F: FnMut(&Expression)>(expr: &Expression, visit: &mut F) {
    visit(expr);
    // Delegate deep traversal to the shared walker by feeding each
    // compound's children. To avoid duplicating the match, lean on
    // `for_each_expression` over a synthetic single-attribute body is
    // overkill; instead recurse through the common compound forms.
    match expr {
        Expression::Array(a) => a.iter().for_each(|e| visit_expr(e, visit)),
        Expression::Object(o) => {
            for (k, v) in o.iter() {
                if let hcl_edit::expr::ObjectKey::Expression(ke) = k {
                    visit_expr(ke, visit);
                }
                visit_expr(v.expr(), visit);
            }
        }
        Expression::Parenthesis(p) => visit_expr(p.inner(), visit),
        Expression::Conditional(c) => {
            visit_expr(&c.cond_expr, visit);
            visit_expr(&c.true_expr, visit);
            visit_expr(&c.false_expr, visit);
        }
        Expression::FuncCall(call) => call.args.iter().for_each(|a| visit_expr(a, visit)),
        Expression::Traversal(t) => {
            visit_expr(&t.expr, visit);
            for op in t.operators.iter() {
                if let TraversalOperator::Index(idx) = op.value() {
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
            if let Some(k) = f.key_expr.as_ref() {
                visit_expr(k, visit);
            }
            visit_expr(&f.value_expr, visit);
            if let Some(c) = f.cond.as_ref() {
                visit_expr(&c.expr, visit);
            }
        }
        Expression::StringTemplate(t) => {
            for el in t.iter() {
                if let Element::Interpolation(i) = el {
                    visit_expr(&i.expr, visit);
                }
            }
        }
        Expression::HeredocTemplate(h) => {
            for el in h.template.iter() {
                if let Element::Interpolation(i) = el {
                    visit_expr(&i.expr, visit);
                }
            }
        }
        _ => {}
    }
}

/// `Some("x")` for a `local.x[...]` traversal.
fn referenced_local(expr: &Expression) -> Option<&str> {
    let Expression::Traversal(t) = expr else {
        return None;
    };
    let Expression::Variable(v) = &t.expr else {
        return None;
    };
    if v.as_str() != "local" {
        return None;
    }
    match t.operators.first().map(|o| o.value()) {
        Some(TraversalOperator::GetAttr(ident)) => Some(ident.as_str()),
        _ => None,
    }
}

/// DFS from `start`; if it can reach itself, return the cycle path
/// (`[a, b, a]`-style, ending where it closed).
fn find_cycle_from(start: &str, defs: &HashMap<String, LocalDef>) -> Option<Vec<String>> {
    let mut stack: Vec<String> = Vec::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    if dfs(start, defs, &mut stack, &mut on_stack, &mut visited) {
        Some(stack)
    } else {
        None
    }
}

fn dfs(
    node: &str,
    defs: &HashMap<String, LocalDef>,
    stack: &mut Vec<String>,
    on_stack: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) -> bool {
    stack.push(node.to_string());
    on_stack.insert(node.to_string());
    if let Some(def) = defs.get(node) {
        for dep in &def.deps {
            if !defs.contains_key(dep) {
                continue; // references a non-local; ignore.
            }
            if on_stack.contains(dep) {
                // Trim the stack to the cycle starting at `dep`.
                if let Some(pos) = stack.iter().position(|n| n == dep) {
                    stack.drain(..pos);
                }
                return true;
            }
            if visited.insert(dep.clone()) && dfs(dep, defs, stack, on_stack, visited) {
                return true;
            }
        }
    }
    on_stack.remove(node);
    stack.pop();
    false
}

fn render_path(path: &[String]) -> String {
    let mut parts: Vec<String> = path.iter().map(|n| format!("local.{n}")).collect();
    if let Some(first) = parts.first().cloned() {
        parts.push(first); // close the loop visually
    }
    parts.join(" -> ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        cyclic_locals_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_self_reference() {
        let d = diags("locals {\n  a = local.a + 1\n}\n");
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(d[0].message.contains("local.a"), "got: {}", d[0].message);
    }

    #[test]
    fn flags_mutual_cycle() {
        let d = diags("locals {\n  a = local.b\n  b = local.a\n}\n");
        assert_eq!(d.len(), 2, "both members flagged: {d:?}");
    }

    #[test]
    fn flags_three_node_cycle() {
        let d = diags("locals {\n  a = local.b\n  b = local.c\n  c = local.a\n}\n");
        assert_eq!(d.len(), 3, "got: {d:?}");
    }

    #[test]
    fn silent_for_acyclic_chain() {
        let d = diags("locals {\n  a = 1\n  b = local.a\n  c = local.b\n}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_non_local_references() {
        let d = diags("locals {\n  a = var.x\n  b = local.a\n}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn cycle_message_renders_path() {
        let d = diags("locals {\n  a = local.b\n  b = local.a\n}\n");
        assert!(
            d.iter().any(|x| x.message.contains("local.a -> local.b -> local.a")
                || x.message.contains("local.b -> local.a -> local.b")),
            "got: {:?}",
            d.iter().map(|x| &x.message).collect::<Vec<_>>()
        );
    }
}
