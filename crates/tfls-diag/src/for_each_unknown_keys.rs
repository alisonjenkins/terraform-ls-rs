//! `terraform_for_each_unknown_keys` — flag a `for_each` / `count` whose
//! **set of keys** (which elements exist) depends on a value not known until
//! apply. Terraform / OpenTofu only errors on this at *plan* time:
//!
//! ```text
//! Invalid for_each argument: ... includes keys or set values from resource
//! attributes that cannot be determined until apply.
//! ```
//!
//! The bug class: the *membership* of the `for_each` map (which keys exist),
//! or the set elements, is derived from a managed-resource attribute / data
//! source that itself depends on a not-yet-created resource. Values being
//! unknown is fine — only the **keys**, the **set elements**, and the **`if`
//! predicate** that filters membership matter.
//!
//! ## Detection heuristic (first cut)
//!
//! Flag a `for_each` / `count` when its **key expression**, its **set
//! element expression**, or its **`if` predicate** transitively references a
//! `<resource_type>.<name>.<attr>` or `data.<...>` value (an apply-time
//! value) rather than only input variables, static locals, or literals.
//!
//! Resolution is field-sensitive through `local.*` and loop variables: a
//! filter that reads a *static* field of an object whose *other* fields embed
//! resource attributes is fine (the membership is still statically known),
//! while a filter that reads the apply-time field is not. This is what
//! distinguishes the broken and fixed forms of the canonical case (an
//! `aws_s3_bucket_policy` whose policy document embedded a not-yet-created
//! IAM role ARN, filtered in the `for_each` `if`).
//!
//! ## Scope / limitations
//!
//! - Resolves `local.*` only within the same file (single body). A `locals`
//!   block in a sibling file is not consulted.
//! - Top-level `resource` / `data` / `module` blocks only (not `dynamic`).
//! - Over-approximates when a loop value is used *whole* (no field access):
//!   conservatively treats it as apply-time if the collection embeds any
//!   resource attribute.

use std::collections::{HashMap, HashSet};

use hcl_edit::expr::{Expression, ObjectKey, TraversalOperator};
use hcl_edit::structure::{Block, Body};
use hcl_edit::Span;
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

/// Reference roots whose values are known at plan time (or whose use here is
/// not the resource/data apply-time class we flag). Everything else with a
/// `<head>.<...>` shape is a managed-resource reference.
const SAFE_ROOTS: &[&str] = &[
    "var",
    "path",
    "terraform",
    "self",
    "each",
    "count",
    "module",
    "local", // handled explicitly (resolved), listed for clarity
];

/// Single-body entry point: resolves `local.*` only within `body`. Kept for
/// tests and callers without module context.
pub fn for_each_unknown_keys_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    for_each_unknown_keys_diagnostics_with_locals(body, rope, &HashMap::new())
}

/// Module-aware entry point: `module_locals` carries the `local.*` definitions
/// aggregated across every `.tf` file in the active module's directory (a
/// `locals` block typically lives in a different file from the `for_each` that
/// reads it). `body`'s own locals are overlaid on top so they always resolve
/// even if `body` is not yet present in the aggregated set.
pub fn for_each_unknown_keys_diagnostics_with_locals(
    body: &Body,
    rope: &Rope,
    module_locals: &HashMap<String, Expression>,
) -> Vec<Diagnostic> {
    let mut locals = module_locals.clone();
    for (name, def) in collect_locals(body) {
        locals.insert(name, def);
    }
    let ctx = Ctx { locals: &locals };
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if !matches!(block.ident.as_str(), "resource" | "data" | "module") {
            continue;
        }
        check_block(block, rope, &ctx, &mut out);
    }
    out
}

struct Ctx<'a> {
    locals: &'a HashMap<String, Expression>,
}

/// How a loop variable is bound to its collection.
#[derive(Clone, Copy)]
enum Bind<'a> {
    /// Bound to element *values* of the collection (`for k, v in C` → `v`,
    /// or `for v in C` → `v`).
    Value(&'a Expression),
    /// Bound to element *keys* of the collection (`for k, v in C` → `k`).
    Key(&'a Expression),
}

type Binds<'a> = HashMap<String, Bind<'a>>;

/// Collect the `local.*` definitions declared in `body`, cloning each value so
/// the map is owned (definitions from sibling files must outlive the borrowed
/// body they came from when aggregated across a module).
pub fn collect_locals(body: &Body) -> HashMap<String, Expression> {
    let mut map = HashMap::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "locals" {
            continue;
        }
        for entry in block.body.iter() {
            if let Some(attr) = entry.as_attribute() {
                // First definition wins (a duplicate is a separate error).
                map.entry(attr.key.as_str().to_string())
                    .or_insert_with(|| attr.value.clone());
            }
        }
    }
    map
}

fn check_block(block: &Block, rope: &Rope, ctx: &Ctx, out: &mut Vec<Diagnostic>) {
    for entry in block.body.iter() {
        let Some(attr) = entry.as_attribute() else {
            continue;
        };
        let kind = match attr.key.as_str() {
            "for_each" => MetaKind::ForEach,
            "count" => MetaKind::Count,
            _ => continue,
        };
        if membership_apply_time(&attr.value, kind, ctx) {
            let range = expr_range(&attr.value, rope);
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message: kind.message().to_string(),
                ..Default::default()
            });
        }
    }
}

#[derive(Clone, Copy)]
enum MetaKind {
    ForEach,
    Count,
}

impl MetaKind {
    fn message(self) -> &'static str {
        match self {
            MetaKind::ForEach => {
                "`for_each` membership depends on a value not known until apply — \
                 Terraform rejects this at plan time. Key it on a statically-known \
                 attribute instead (values may stay unknown; only the keys must be known)."
            }
            MetaKind::Count => {
                "`count` depends on a value not known until apply — Terraform rejects \
                 this at plan time. Base the count on a statically-known value instead."
            }
        }
    }
}

/// Whether the membership-determining part of a `for_each` / `count` value is
/// apply-time-derived.
///
/// `visited` is **path-scoped**: it tracks the `local.*` names on the current
/// resolution chain to break cycles, and is cloned (never shared mutably) when
/// descending into independent sub-expressions — so resolving the same local
/// twice across sibling expressions (e.g. once for the key, once for the `if`)
/// does not poison the second resolution.
fn membership_apply_time(value: &Expression, kind: MetaKind, ctx: &Ctx) -> bool {
    let visited = HashSet::new();
    let binds = Binds::new();
    membership_apply_time_inner(value, kind, ctx, &binds, &visited)
}

fn membership_apply_time_inner(
    value: &Expression,
    kind: MetaKind,
    ctx: &Ctx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    match value {
        // `count = length(<arg>)` — the count equals the number of elements of
        // `<arg>`, so the same membership analysis as a `for_each` applies.
        Expression::FuncCall(call)
            if matches!(kind, MetaKind::Count) && call.name.name.as_str() == "length" =>
        {
            match call.args.get(0) {
                Some(arg) => {
                    membership_apply_time_inner(arg, MetaKind::ForEach, ctx, binds, visited)
                }
                None => false,
            }
        }
        // `for_each` / nested set-builder over a comprehension.
        Expression::ForExpr(fe) => {
            let mut inner = binds.clone();
            let coll = &fe.intro.collection_expr;
            inner.insert(fe.intro.value_var.as_str().to_string(), Bind::Value(coll));
            if let Some(kv) = fe.intro.key_var.as_ref() {
                inner.insert(kv.as_str().to_string(), Bind::Key(coll));
            }
            // The key set is determined by the key expression (map-for) or the
            // value/element expression (set-for), plus any `if` filter.
            let membership_expr = fe.key_expr.as_ref().unwrap_or(&fe.value_expr);
            if references_apply_time(membership_expr, ctx, &inner, visited) {
                return true;
            }
            if let Some(cond) = fe.cond.as_ref() {
                if references_apply_time(&cond.expr, ctx, &inner, visited) {
                    return true;
                }
            }
            false
        }
        // Set/map builder wrappers: `toset([...])`, `tomap({...})`,
        // `setunion(...)`, `concat(...)`, etc. Recurse into the arguments.
        Expression::FuncCall(call) if is_collection_builder(call.name.name.as_str()) => call
            .args
            .iter()
            .any(|arg| membership_apply_time_inner(arg, kind, ctx, binds, visited)),
        // A literal array of set elements — each element is a key.
        Expression::Array(arr) => arr
            .iter()
            .any(|el| references_apply_time(el, ctx, binds, visited)),
        // A bare `local.X` reference — resolve and re-analyse its definition.
        Expression::Traversal(_) if local_name(value).is_some() => {
            match resolve_local_def(local_name(value), ctx, visited) {
                Some((def, v2)) => membership_apply_time_inner(def, kind, ctx, binds, &v2),
                None => false,
            }
        }
        // Any other shape (a direct reference, conditional, etc.): the whole
        // value's key set is apply-time iff the value references an apply-time
        // attribute.
        other => references_apply_time(other, ctx, binds, visited),
    }
}

fn is_collection_builder(name: &str) -> bool {
    matches!(
        name,
        "toset"
            | "tomap"
            | "tolist"
            | "setunion"
            | "setintersection"
            | "setproduct"
            | "setsubtract"
            | "concat"
            | "merge"
            | "flatten"
            | "keys"
            | "distinct"
            | "compact"
            | "sort"
    )
}

/// Whether `expr` transitively references an apply-time (resource / data)
/// value, resolving `local.*` and loop-variable bindings field-sensitively.
fn references_apply_time(
    expr: &Expression,
    ctx: &Ctx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    match expr {
        Expression::Null(_)
        | Expression::Bool(_)
        | Expression::Number(_)
        | Expression::String(_) => false,
        // A bare identifier — possibly a loop variable used whole.
        Expression::Variable(v) => match binds.get(v.as_str()) {
            Some(bind) => bind_apply_time(*bind, None, ctx, binds, visited),
            None => false,
        },
        Expression::Array(arr) => arr
            .iter()
            .any(|e| references_apply_time(e, ctx, binds, visited)),
        Expression::Object(obj) => obj.iter().any(|(key, val)| {
            let key_hit = matches!(key, ObjectKey::Expression(k)
                if references_apply_time(k, ctx, binds, visited));
            key_hit || references_apply_time(val.expr(), ctx, binds, visited)
        }),
        Expression::Parenthesis(p) => references_apply_time(p.inner(), ctx, binds, visited),
        Expression::Conditional(c) => {
            references_apply_time(&c.cond_expr, ctx, binds, visited)
                || references_apply_time(&c.true_expr, ctx, binds, visited)
                || references_apply_time(&c.false_expr, ctx, binds, visited)
        }
        Expression::UnaryOp(op) => references_apply_time(&op.expr, ctx, binds, visited),
        Expression::BinaryOp(op) => {
            references_apply_time(&op.lhs_expr, ctx, binds, visited)
                || references_apply_time(&op.rhs_expr, ctx, binds, visited)
        }
        Expression::FuncCall(call) => {
            // `lookup(<loop_var>, "field", <default>)` reads a specific field
            // of the loop value — resolve field-sensitively so a lookup of a
            // static field isn't tarred with an apply-time sibling field.
            if call.name.name.as_str() == "lookup" {
                if let (Some(Expression::Variable(var)), Some(Expression::String(field))) =
                    (call.args.get(0), call.args.get(1))
                {
                    if let Some(bind) = binds.get(var.as_str()) {
                        let hit = bind_apply_time(
                            *bind,
                            Some(field.value().as_str()),
                            ctx,
                            binds,
                            visited,
                        );
                        // The default (3rd arg) is also part of the value, but
                        // does not affect membership unless it is apply-time.
                        return hit
                            || call
                                .args
                                .get(2)
                                .is_some_and(|d| references_apply_time(d, ctx, binds, visited));
                    }
                }
            }
            call.args
                .iter()
                .any(|arg| references_apply_time(arg, ctx, binds, visited))
        }
        Expression::Traversal(t) => traversal_apply_time(t, ctx, binds, visited),
        Expression::ForExpr(fe) => {
            let mut inner = binds.clone();
            let coll = &fe.intro.collection_expr;
            inner.insert(fe.intro.value_var.as_str().to_string(), Bind::Value(coll));
            if let Some(kv) = fe.intro.key_var.as_ref() {
                inner.insert(kv.as_str().to_string(), Bind::Key(coll));
            }
            references_apply_time(coll, ctx, binds, visited)
                || references_apply_time(&fe.value_expr, ctx, &inner, visited)
                || fe
                    .key_expr
                    .as_ref()
                    .is_some_and(|k| references_apply_time(k, ctx, &inner, visited))
                || fe
                    .cond
                    .as_ref()
                    .is_some_and(|c| references_apply_time(&c.expr, ctx, &inner, visited))
        }
        // Templates: interpolations / directives carry expressions.
        Expression::StringTemplate(tpl) => template_apply_time(tpl.iter(), ctx, binds, visited),
        Expression::HeredocTemplate(h) => {
            template_apply_time(h.template.iter(), ctx, binds, visited)
        }
    }
}

fn template_apply_time<'a, I>(
    elements: I,
    ctx: &Ctx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool
where
    I: IntoIterator<Item = &'a hcl_edit::template::Element>,
{
    use hcl_edit::template::{Directive, Element};
    elements.into_iter().any(|element| match element {
        Element::Literal(_) => false,
        Element::Interpolation(interp) => references_apply_time(&interp.expr, ctx, binds, visited),
        Element::Directive(directive) => match directive.as_ref() {
            Directive::If(i) => {
                references_apply_time(&i.if_expr.cond_expr, ctx, binds, visited)
                    || template_apply_time(i.if_expr.template.iter(), ctx, binds, visited)
                    || i.else_expr.as_ref().is_some_and(|e| {
                        template_apply_time(e.template.iter(), ctx, binds, visited)
                    })
            }
            Directive::For(f) => {
                references_apply_time(&f.for_expr.collection_expr, ctx, binds, visited)
                    || template_apply_time(f.for_expr.template.iter(), ctx, binds, visited)
            }
        },
    })
}

fn traversal_apply_time(
    t: &hcl_edit::expr::Traversal,
    ctx: &Ctx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    let Expression::Variable(head) = &t.expr else {
        // Head is a complex expression (e.g. `func(...).attr`). Recurse into
        // the head and any dynamic index operators.
        return references_apply_time(&t.expr, ctx, binds, visited)
            || index_operators_apply_time(t, ctx, binds, visited);
    };
    let head = head.as_str();

    // `local.<name>[...]` — resolve the local and recurse into its definition.
    if head == "local" {
        return match resolve_local_def(first_attr(t), ctx, visited) {
            Some((def, v2)) => references_apply_time(def, ctx, binds, &v2),
            None => false,
        };
    }
    // `data.<...>` is the apply-time class we flag explicitly.
    if head == "data" {
        return true;
    }
    // A loop variable with a field access — resolve field-sensitively.
    if let Some(bind) = binds.get(head) {
        return bind_apply_time(*bind, first_attr(t), ctx, binds, visited);
    }
    // Other safe roots (`var`, `path`, `count`, ...): the reference itself is
    // plan-known, but a dynamic index might not be.
    if SAFE_ROOTS.contains(&head) {
        return index_operators_apply_time(t, ctx, binds, visited);
    }
    // Anything else is a managed-resource reference: `<type>.<name>.<attr>`.
    true
}

fn index_operators_apply_time(
    t: &hcl_edit::expr::Traversal,
    ctx: &Ctx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    t.operators.iter().any(|op| {
        if let TraversalOperator::Index(idx) = op.value() {
            references_apply_time(idx, ctx, binds, visited)
        } else {
            false
        }
    })
}

/// Resolve whether the bound loop value (optionally a specific field of it) is
/// apply-time-derived.
fn bind_apply_time(
    bind: Bind,
    field: Option<&str>,
    ctx: &Ctx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    match bind {
        Bind::Value(coll) => element_value_apply_time(coll, field, ctx, binds, visited),
        // Keys: a static object's keys are literal strings; a set/list's keys
        // are its elements; a resource-derived collection's keys are unknown.
        Bind::Key(coll) => key_set_apply_time(coll, ctx, binds, visited),
    }
}

/// Whether element *values* (optionally a specific field) of `coll` are
/// apply-time.
fn element_value_apply_time(
    coll: &Expression,
    field: Option<&str>,
    ctx: &Ctx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    let (resolved, v2) = resolve_through_locals(coll, ctx, visited);
    match resolved {
        Expression::Object(obj) => obj
            .iter()
            .any(|(_k, v)| element_field_apply_time(v.expr(), field, ctx, binds, &v2)),
        Expression::Array(arr) => arr
            .iter()
            .any(|el| element_field_apply_time(el, field, ctx, binds, &v2)),
        other => references_apply_time(other, ctx, binds, &v2),
    }
}

fn element_field_apply_time(
    elem: &Expression,
    field: Option<&str>,
    ctx: &Ctx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    match field {
        None => references_apply_time(elem, ctx, binds, visited),
        Some(f) => match elem {
            Expression::Object(inner) => match object_get(inner, f) {
                Some(fv) => references_apply_time(fv, ctx, binds, visited),
                // Field absent on this element — contributes nothing.
                None => false,
            },
            // Not an object but a field was read — fall back to the whole
            // element (e.g. the element is itself a `local.*` reference).
            _ => references_apply_time(elem, ctx, binds, visited),
        },
    }
}

fn key_set_apply_time(
    coll: &Expression,
    ctx: &Ctx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    let (resolved, v2) = resolve_through_locals(coll, ctx, visited);
    match resolved {
        // Literal object: keys are static strings / idents.
        Expression::Object(_) => false,
        // Set/list: the elements are the keys.
        Expression::Array(arr) => arr
            .iter()
            .any(|el| references_apply_time(el, ctx, binds, &v2)),
        other => references_apply_time(other, ctx, binds, &v2),
    }
}

/// Follow a chain of `local.*` references to the underlying definition,
/// stopping at the first non-local expression (or a cycle). Returns the
/// resolved expression and the `visited` set extended with every local name
/// traversed (so callers guard cycles when recursing into the result).
fn resolve_through_locals<'a>(
    expr: &'a Expression,
    ctx: &'a Ctx<'a>,
    visited: &HashSet<String>,
) -> (&'a Expression, HashSet<String>) {
    let mut cur = expr;
    let mut v = visited.clone();
    while let Some(name) = local_name(cur) {
        if !v.insert(name.to_string()) {
            break; // cycle
        }
        match ctx.locals.get(name) {
            Some(def) => cur = def,
            None => break,
        }
    }
    (cur, v)
}

/// Resolve a single `local.<name>` to its definition, returning the definition
/// and the `visited` set extended with `<name>`. `None` on a cycle, a missing
/// name, or a `None` input.
fn resolve_local_def<'a>(
    name: Option<&str>,
    ctx: &Ctx<'a>,
    visited: &HashSet<String>,
) -> Option<(&'a Expression, HashSet<String>)> {
    let name = name?;
    if visited.contains(name) {
        return None; // cycle
    }
    let def = ctx.locals.get(name)?;
    let mut v = visited.clone();
    v.insert(name.to_string());
    Some((def, v))
}

/// If `expr` is a `local.<name>` traversal, return `<name>`.
fn local_name(expr: &Expression) -> Option<&str> {
    let Expression::Traversal(t) = expr else {
        return None;
    };
    let Expression::Variable(head) = &t.expr else {
        return None;
    };
    if head.as_str() != "local" {
        return None;
    }
    first_attr(t)
}

/// The first `.attr` of a traversal (`local.items.a` → `"items"`).
fn first_attr(t: &hcl_edit::expr::Traversal) -> Option<&str> {
    t.operators.iter().find_map(|op| {
        if let TraversalOperator::GetAttr(ident) = op.value() {
            Some(ident.as_str())
        } else {
            None
        }
    })
}

fn object_get<'a>(obj: &'a hcl_edit::expr::Object, name: &str) -> Option<&'a Expression> {
    obj.iter().find_map(|(key, val)| {
        let matches = match key {
            ObjectKey::Ident(i) => i.as_str() == name,
            ObjectKey::Expression(Expression::String(s)) => s.value().as_str() == name,
            _ => false,
        };
        matches.then(|| val.expr())
    })
}

fn expr_range(expr: &Expression, rope: &Rope) -> Range {
    expr.span()
        .and_then(|sp| hcl_span_to_lsp_range(rope, sp).ok())
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        for_each_unknown_keys_diagnostics(&body, &rope)
    }

    fn flagged(src: &str) -> bool {
        !diags(src).is_empty()
    }

    const BROKEN: &str = r#"
resource "aws_iam_role" "r" {
  name               = "example"
  assume_role_policy = "{}"
}

locals {
  items = {
    "a" = { policy = aws_iam_role.r.arn }
    "b" = {}
  }
}

resource "null_resource" "broken" {
  for_each = { for k, v in local.items : k => v if lookup(v, "policy", "") != "" }
}
"#;

    const FIXED: &str = r#"
resource "aws_iam_role" "r" {
  name               = "example"
  assume_role_policy = "{}"
}

locals {
  items_fixed = {
    "a" = { has_policy = true, policy = aws_iam_role.r.arn }
    "b" = {}
  }
}

resource "null_resource" "fixed" {
  for_each = { for k, v in local.items_fixed : k => v if lookup(v, "has_policy", false) }
}
"#;

    #[test]
    fn flags_broken_minimal_recreation() {
        assert!(flagged(BROKEN), "broken for_each should be flagged");
    }

    #[test]
    fn silent_for_fixed_static_filter() {
        let d = diags(FIXED);
        assert!(d.is_empty(), "fixed for_each should be silent; got: {d:?}");
    }

    #[test]
    fn flags_canonical_resource_id_keys() {
        // `for_each = { for s in aws_subnet.all : s.id => s }` — keys are
        // resource IDs, unknown until apply.
        let src = r#"
resource "null_resource" "x" {
  for_each = { for s in aws_subnet.all : s.id => s }
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_input_var_id_keys() {
        // Same shape but over an input variable — keys are plan-known.
        let src = r#"
resource "null_resource" "x" {
  for_each = { for s in var.subnets : s.id => s }
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_toset_of_resource_ids() {
        let src = r#"
resource "null_resource" "x" {
  for_each = toset([for s in aws_subnet.all : s.id])
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_plain_var_map() {
        let src = r#"
resource "null_resource" "x" {
  for_each = var.instances
}
"#;
        assert!(!flagged(src));
    }

    #[test]
    fn flags_plain_resource_reference() {
        let src = r#"
resource "null_resource" "x" {
  for_each = aws_subnet.all
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn flags_data_source_in_filter() {
        let src = r#"
resource "null_resource" "x" {
  for_each = { for k, v in var.items : k => v if data.aws_iam_role.r.arn != "" }
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn flags_count_length_of_resource_comprehension() {
        let src = r#"
resource "null_resource" "x" {
  count = length([for s in aws_subnet.all : s.id])
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_count_length_of_var() {
        let src = r#"
resource "null_resource" "x" {
  count = length(var.list)
}
"#;
        assert!(!flagged(src));
    }

    #[test]
    fn silent_for_count_conditional() {
        let src = r#"
resource "null_resource" "x" {
  count = var.enabled ? 1 : 0
}
"#;
        assert!(!flagged(src));
    }

    #[test]
    fn silent_for_static_local_filter() {
        // Filter reads a fully-static local — no resource refs anywhere.
        let src = r#"
locals {
  items = {
    "a" = { enabled = true }
    "b" = { enabled = false }
  }
}
resource "null_resource" "x" {
  for_each = { for k, v in local.items : k => v if v.enabled }
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn cyclic_locals_terminate() {
        // Self-referential locals must not loop forever.
        let src = r#"
locals {
  a = local.b
  b = local.a
}
resource "null_resource" "x" {
  for_each = local.a
}
"#;
        // Just assert it returns (no hang / no panic).
        let _ = diags(src);
    }

    #[test]
    fn flags_field_access_via_dot() {
        // `v.policy` (dot access, not lookup) on an apply-time field.
        let src = r#"
locals {
  items = {
    "a" = { policy = aws_iam_role.r.arn }
  }
}
resource "null_resource" "x" {
  for_each = { for k, v in local.items : k => v if v.policy != "" }
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn resolves_locals_from_sibling_file() {
        // The `locals` block and the resource it embeds live in a *different*
        // file than the `for_each` that reads them — the single-body path
        // can't see this; the module-aware path must.
        let sibling = r#"
resource "aws_iam_role" "r" {
  name               = "x"
  assume_role_policy = "{}"
}
locals {
  items = {
    "a" = { policy = aws_iam_role.r.arn }
    "b" = {}
  }
}
"#;
        let main = r#"
resource "null_resource" "broken" {
  for_each = { for k, v in local.items : k => v if lookup(v, "policy", "") != "" }
}
"#;
        let sibling_body = parse_source(sibling).body.expect("parses");
        let module_locals = collect_locals(&sibling_body);
        let rope = Rope::from_str(main);
        let body = parse_source(main).body.expect("parses");

        // Single-body path is blind to the sibling local — no diagnostic.
        assert!(
            for_each_unknown_keys_diagnostics(&body, &rope).is_empty(),
            "single-body path should not resolve a sibling local"
        );
        // Module-aware path resolves it and flags.
        let d = for_each_unknown_keys_diagnostics_with_locals(&body, &rope, &module_locals);
        assert!(
            !d.is_empty(),
            "sibling-file local should resolve; got: {d:?}"
        );
    }

    #[test]
    fn only_resource_data_module_blocks() {
        // A `for_each` in some other top-level block kind is ignored.
        let src = r#"
output "x" {
  value = { for s in aws_subnet.all : s.id => s }
}
"#;
        assert!(!flagged(src));
    }
}
