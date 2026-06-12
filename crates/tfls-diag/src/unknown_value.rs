//! Shared "unknown until apply" expression analysis.
//!
//! Several Terraform config positions must be **plan-time known** — Terraform
//! rejects an unknown ("known after apply") value there with a hard plan-time
//! error rather than deferring it:
//!
//! - `for_each` map keys / set elements and `count` (values may stay unknown;
//!   only the membership must be known) — `terraform_for_each_unknown_keys`.
//! - `import` block `id` / `for_each` (Terraform 1.5+ / 1.7+) —
//!   `terraform_import_unknown_id`.
//!
//! This module holds the rule-agnostic analysis: given an expression, decide
//! whether its value (or its key-set membership) transitively derives from an
//! apply-time source. Resolution is field-sensitive through `local.*` and
//! comprehension loop variables, and module-aware via [`ModuleUnknownInputs`]
//! (the definitions a rule needs usually live in sibling `.tf` files).
//!
//! ## What counts as apply-time
//!
//! - A managed-resource attribute (`<type>.<name>.<attr>`) — unknown when
//!   computed and the resource has pending changes. An attribute set
//!   explicitly in the resource's config resolves transitively to the config
//!   expression; computed collections in [`PLAN_KNOWN_COMPUTED_COLLECTIONS`]
//!   are plan-known per their listed fields. Everything else flags.
//! - `data.<...>` — only when the data source's *own* configuration is
//!   apply-time or it carries a `depends_on` on a managed resource (a data
//!   read is otherwise performed during plan and its attributes are known).
//!
//! `var`, `path`, `terraform`, `self`, `each`, `count`, `module` roots are
//! plan-known. When something can't be resolved, prefer staying silent
//! (false negatives over false positives).

use std::collections::{HashMap, HashSet};

use hcl_edit::expr::{Expression, ObjectKey, TraversalOperator};
use hcl_edit::structure::Body;
use hcl_edit::Span;
use lsp_types::Range;
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

/// Reference roots whose values are known at plan time (or whose use here is
/// not the resource/data apply-time class we flag). Everything else with a
/// `<head>.<...>` shape is a managed-resource reference.
const SAFE_ROOTS: &[&str] = &[
    "path",
    "terraform",
    "self",
    "each",
    "count",
    "var",   // handled explicitly (caller-passed unknownness), listed for clarity
    "local", // handled explicitly (resolved), listed for clarity
];

/// Resolve whether a `module.<label>.<output>` reference is apply-time by
/// analysing the output's defining expression in the child module. Injected
/// by the LSP layer (it owns module-source resolution and the document
/// store); rules without one treat every module output as plan-known
/// (the pre-existing behaviour).
pub trait ModuleOutputLookup {
    /// `rest` is the GetAttr path after the output name
    /// (`module.m.out.field` → `["field"]`) for field-sensitive resolution
    /// of object-literal outputs. `None` = unresolvable → stay silent.
    fn output_apply_time(&self, module_label: &str, output: &str, rest: &[&str]) -> Option<bool>;
}

/// Computed collections that providers populate at *plan* time (via
/// `CustomizeDiff`) even though their schema marks them computed. Tuple:
/// `(resource_type, attribute, element fields plan-known)`. For these,
/// collection *membership* (element count / set identity derived from the
/// listed fields) is plan-known; fields NOT listed stay apply-time.
///
/// The canonical case: `aws_acm_certificate.domain_validation_options` —
/// the AWS provider derives one element per (config-known) domain name at
/// plan, with `domain_name` known and `resource_record_*` unknown. Keying a
/// `for_each` on `dvo.domain_name` is therefore plan-valid.
const PLAN_KNOWN_COMPUTED_COLLECTIONS: &[(&str, &str, &[&str])] = &[(
    "aws_acm_certificate",
    "domain_validation_options",
    &["domain_name"],
)];

/// An allowlisted plan-known computed collection reference.
struct AllowlistedUse {
    known_fields: &'static [&'static str],
    /// `Some(unknown)` when the traversal selects element field(s) beyond the
    /// collection attribute (e.g. `...[*].domain_name`): `unknown` is true if
    /// any selected field is not plan-known. `None` for a bare collection
    /// reference.
    tail: Option<bool>,
}

/// Match a traversal against [`PLAN_KNOWN_COMPUTED_COLLECTIONS`]:
/// `<type>.<name>.<attr>` with optional trailing splat / index / field
/// operators.
fn allowlisted_traversal(t: &hcl_edit::expr::Traversal) -> Option<AllowlistedUse> {
    let Expression::Variable(head) = &t.expr else {
        return None;
    };
    let head = head.as_str();
    if head == "data" || SAFE_ROOTS.contains(&head) {
        return None;
    }
    let mut get_attrs = t.operators.iter().filter_map(|op| match op.value() {
        TraversalOperator::GetAttr(ident) => Some(ident.as_str()),
        _ => None,
    });
    let _name = get_attrs.next()?;
    let attr = get_attrs.next()?;
    let known_fields = PLAN_KNOWN_COMPUTED_COLLECTIONS
        .iter()
        .find(|(rtype, rattr, _)| *rtype == head && *rattr == attr)
        .map(|(_, _, fields)| *fields)?;
    let mut tail = None;
    for field in get_attrs {
        let unknown = !known_fields.contains(&field);
        tail = Some(tail.unwrap_or(false) || unknown);
    }
    Some(AllowlistedUse { known_fields, tail })
}

/// The plan-relevant configuration of a single `resource` / `data` block,
/// collected so references to it can be resolved transitively.
#[derive(Debug, Clone, Default)]
pub struct BlockConfig {
    /// Top-level attribute expressions, keyed by attribute name. Meta-args
    /// (`count`, `for_each`, `depends_on`, `provider`, `lifecycle`) excluded.
    pub attrs: HashMap<String, Expression>,
    /// Attribute expressions from nested blocks (e.g. a data source's
    /// `filter { ... }`), flattened — only "does any config expression
    /// reference an apply-time value" questions are asked of these.
    pub nested_exprs: Vec<Expression>,
    /// Whether the block has a non-empty `depends_on` naming a managed
    /// resource — for a data source this defers the read to apply.
    pub has_depends_on: bool,
}

/// What a caller passes into a module variable, as far as plan-time
/// knownness goes. Both bits matter independently: a caller-passed map with
/// statically-known keys but apply-time *values* is still a valid `for_each`.
#[derive(Debug, Clone, Default)]
pub struct UnknownVarInfo {
    /// The membership (map keys / set elements / length) of the passed value
    /// is apply-time.
    pub membership: bool,
    /// The passed value itself is apply-time.
    pub value: bool,
    /// Human-readable origin (names the caller), appended to rule messages.
    pub reason: String,
}

/// Module-wide inputs the unknown-value analysis resolves through: `local.*`
/// definitions plus `resource` / `data` block configurations, aggregated
/// across every `.tf` file in the active module's directory (the definitions
/// usually live in different files than the expression referencing them).
#[derive(Debug, Clone, Default)]
pub struct ModuleUnknownInputs {
    /// `local.<name>` → defining expression.
    pub locals: HashMap<String, Expression>,
    /// `(data_type, name)` → config of that `data` block.
    pub data_configs: HashMap<(String, String), BlockConfig>,
    /// `(resource_type, name)` → config of that `resource` block.
    pub resource_configs: HashMap<(String, String), BlockConfig>,
    /// `var.<name>` entries a CALLER passes an apply-time value into.
    /// Empty unless the LSP layer fills it (`collect_module_inputs` cannot
    /// see callers). Absence of a name means plan-known.
    pub unknown_variables: HashMap<String, UnknownVarInfo>,
}

impl ModuleUnknownInputs {
    /// Merge `other` in, keeping existing entries on collision (first
    /// definition wins — a duplicate is a separate error).
    pub fn merge_missing(&mut self, other: ModuleUnknownInputs) {
        for (k, v) in other.locals {
            self.locals.entry(k).or_insert(v);
        }
        for (k, v) in other.data_configs {
            self.data_configs.entry(k).or_insert(v);
        }
        for (k, v) in other.resource_configs {
            self.resource_configs.entry(k).or_insert(v);
        }
        for (k, v) in other.unknown_variables {
            self.unknown_variables.entry(k).or_insert(v);
        }
    }

    /// Merge `other` in, letting `other` override on collision (used to
    /// overlay the active body's own definitions so they always resolve even
    /// if the body is not yet present in the aggregated set).
    pub fn merge_override(&mut self, other: ModuleUnknownInputs) {
        self.locals.extend(other.locals);
        self.data_configs.extend(other.data_configs);
        self.resource_configs.extend(other.resource_configs);
        self.unknown_variables.extend(other.unknown_variables);
    }
}

/// Collect the unknown-value-analysis inputs declared in one `body`: every
/// `local.*` definition plus every `resource` / `data` block config. Values
/// are cloned so the result is owned (definitions from sibling files must
/// outlive the borrowed body they came from when aggregated across a module).
pub fn collect_module_inputs(body: &Body) -> ModuleUnknownInputs {
    let mut out = ModuleUnknownInputs::default();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        match block.ident.as_str() {
            "locals" => {
                for entry in block.body.iter() {
                    if let Some(attr) = entry.as_attribute() {
                        // First definition wins (a duplicate is a separate error).
                        out.locals
                            .entry(attr.key.as_str().to_string())
                            .or_insert_with(|| attr.value.clone());
                    }
                }
            }
            kind @ ("resource" | "data") => {
                let labels: Vec<&str> = block.labels.iter().map(|l| l.as_str()).collect();
                let [type_name, name] = labels.as_slice() else {
                    continue;
                };
                let mut config = BlockConfig::default();
                for entry in block.body.iter() {
                    if let Some(nested) = entry.as_block() {
                        if nested.ident.as_str() != "lifecycle" {
                            collect_nested_exprs(&nested.body, &mut config.nested_exprs);
                        }
                        continue;
                    }
                    let Some(attr) = entry.as_attribute() else {
                        continue;
                    };
                    match attr.key.as_str() {
                        "depends_on" => {
                            config.has_depends_on = depends_on_managed_resource(&attr.value);
                        }
                        "count" | "for_each" | "provider" => {}
                        other => {
                            config
                                .attrs
                                .entry(other.to_string())
                                .or_insert_with(|| attr.value.clone());
                        }
                    }
                }
                let map = if kind == "data" {
                    &mut out.data_configs
                } else {
                    &mut out.resource_configs
                };
                map.entry((type_name.to_string(), name.to_string()))
                    .or_insert(config);
            }
            _ => {}
        }
    }
    out
}

/// Collect the `local.*` definitions declared in `body`. Retained as the
/// narrow form of [`collect_module_inputs`] for callers that only need
/// locals.
pub fn collect_locals(body: &Body) -> HashMap<String, Expression> {
    collect_module_inputs(body).locals
}

/// Flatten every attribute expression under `body` (recursing into nested
/// blocks) into `out`.
fn collect_nested_exprs(body: &Body, out: &mut Vec<Expression>) {
    for entry in body.iter() {
        if let Some(attr) = entry.as_attribute() {
            out.push(attr.value.clone());
        } else if let Some(block) = entry.as_block() {
            collect_nested_exprs(&block.body, out);
        }
    }
}

/// Whether a `depends_on` expression names at least one managed resource
/// (`data.*` / `module.*` targets don't defer a data read the same way).
fn depends_on_managed_resource(expr: &Expression) -> bool {
    let Expression::Array(arr) = expr else {
        return false;
    };
    arr.iter().any(|el| {
        let head = match el {
            Expression::Traversal(t) => match &t.expr {
                Expression::Variable(head) => head.as_str(),
                _ => return false,
            },
            Expression::Variable(v) => v.as_str(),
            _ => return false,
        };
        !matches!(head, "data" | "module")
    })
}

/// Borrowed view over [`ModuleUnknownInputs`] threaded through the analysis,
/// plus the optional provider-schema lookup used to refine resource-attribute
/// classification.
pub struct UnknownCtx<'a> {
    pub locals: &'a HashMap<String, Expression>,
    pub data_configs: &'a HashMap<(String, String), BlockConfig>,
    pub resource_configs: &'a HashMap<(String, String), BlockConfig>,
    pub unknown_variables: &'a HashMap<String, UnknownVarInfo>,
    pub schema: Option<&'a dyn crate::schema_validation::SchemaLookup>,
    pub module_outputs: Option<&'a dyn ModuleOutputLookup>,
}

impl<'a> UnknownCtx<'a> {
    pub fn new(
        inputs: &'a ModuleUnknownInputs,
        schema: Option<&'a dyn crate::schema_validation::SchemaLookup>,
    ) -> Self {
        UnknownCtx {
            locals: &inputs.locals,
            data_configs: &inputs.data_configs,
            resource_configs: &inputs.resource_configs,
            unknown_variables: &inputs.unknown_variables,
            schema,
            module_outputs: None,
        }
    }

    pub fn with_module_outputs(mut self, lookup: Option<&'a dyn ModuleOutputLookup>) -> Self {
        self.module_outputs = lookup;
        self
    }
}

/// How a meta-argument value is consumed — `count = length(x)` makes the
/// *length* of `x` the membership question, so the two analyses differ only
/// at the top level.
#[derive(Clone, Copy)]
pub enum MetaKind {
    ForEach,
    Count,
}

/// How a loop variable is bound to its collection.
#[derive(Clone, Copy)]
pub(crate) enum Bind<'a> {
    /// Bound to element *values* of the collection (`for k, v in C` → `v`,
    /// or `for v in C` → `v`).
    Value(&'a Expression),
    /// Bound to element *keys* of the collection (`for k, v in C` → `k`).
    Key(&'a Expression),
}

pub(crate) type Binds<'a> = HashMap<String, Bind<'a>>;

/// Whether the membership-determining part of a `for_each` / `count` value is
/// apply-time-derived.
///
/// `visited` is **path-scoped**: it tracks the `local.*` names on the current
/// resolution chain to break cycles, and is cloned (never shared mutably) when
/// descending into independent sub-expressions — so resolving the same local
/// twice across sibling expressions (e.g. once for the key, once for the `if`)
/// does not poison the second resolution.
pub fn membership_apply_time(value: &Expression, kind: MetaKind, ctx: &UnknownCtx) -> bool {
    let visited = HashSet::new();
    let binds = Binds::new();
    membership_apply_time_inner(value, kind, ctx, &binds, &visited)
}

/// Whether `value` itself (not just its membership) transitively references
/// an apply-time value. Entry point for scalar positions like an `import`
/// block's `id`.
pub fn value_apply_time(value: &Expression, ctx: &UnknownCtx) -> bool {
    let visited = HashSet::new();
    let binds = Binds::new();
    references_apply_time(value, ctx, &binds, &visited)
}

/// Verdict for a module output's defining expression with a trailing GetAttr
/// path (`module.m.out.field` → `rest = ["field"]`): walk object literals
/// field-sensitively, fall back to the whole-value verdict otherwise. Used by
/// [`ModuleOutputLookup`] implementations; `ctx` must be the CHILD module's
/// context.
pub fn output_expr_apply_time(expr: &Expression, rest: &[&str], ctx: &UnknownCtx) -> bool {
    match rest.split_first() {
        None => value_apply_time(expr, ctx),
        Some((field, remaining)) => match expr {
            Expression::Object(obj) => match object_get(obj, field) {
                Some(field_expr) => output_expr_apply_time(field_expr, remaining, ctx),
                // Absent field — contributes nothing.
                None => false,
            },
            _ => value_apply_time(expr, ctx),
        },
    }
}

fn membership_apply_time_inner(
    value: &Expression,
    kind: MetaKind,
    ctx: &UnknownCtx,
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
                    // Cardinality-only use: an allowlisted plan-known computed
                    // collection has a plan-known *length* even though its
                    // element values are unknown.
                    if let Expression::Traversal(t) = arg {
                        if matches!(allowlisted_traversal(t), Some(a) if a.tail.is_none()) {
                            return false;
                        }
                    }
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
        // A literal map — only the KEYS are membership; values may be
        // unknown.
        Expression::Object(obj) => obj.iter().any(|(key, _val)| {
            matches!(key, ObjectKey::Expression(k)
                if references_apply_time(k, ctx, binds, visited))
        }),
        // A bare `local.X` reference — resolve and re-analyse its definition.
        Expression::Traversal(_) if local_name(value).is_some() => {
            match resolve_local_def(local_name(value), ctx, visited) {
                Some((def, v2)) => membership_apply_time_inner(def, kind, ctx, binds, &v2),
                None => false,
            }
        }
        // A bare `var.X` used as the collection — its membership is what
        // matters (a caller-passed map with static keys and apply-time
        // VALUES is a valid for_each).
        Expression::Traversal(_) if var_name(value).is_some() => var_name(value)
            .and_then(|name| ctx.unknown_variables.get(name))
            .is_some_and(|info| info.membership),
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
pub(crate) fn references_apply_time(
    expr: &Expression,
    ctx: &UnknownCtx,
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
    ctx: &UnknownCtx,
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
    ctx: &UnknownCtx,
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
    // `data.<type>.<name>.<attr>` — a data source is read *during* plan (its
    // attributes are plan-known) unless its own config is apply-time or a
    // `depends_on` on a managed resource defers the read to apply.
    if head == "data" {
        return data_reference_apply_time(t, ctx, binds, visited);
    }
    // `var.<name>[...]` — plan-known unless a caller is known to pass an
    // apply-time value (LSP-filled `unknown_variables`).
    if head == "var" {
        let hit = first_attr(t)
            .and_then(|name| ctx.unknown_variables.get(name))
            .is_some_and(|info| info.value);
        return hit || index_operators_apply_time(t, ctx, binds, visited);
    }
    // `module.<label>.<output>[...]` — resolve the output's defining
    // expression in the child module when a lookup is wired. Unresolvable
    // (no lookup / unknown module / missing output) → silent.
    if head == "module" {
        let mut get_attrs = t.operators.iter().filter_map(|op| match op.value() {
            TraversalOperator::GetAttr(ident) => Some(ident.as_str()),
            _ => None,
        });
        let (Some(label), Some(output)) = (get_attrs.next(), get_attrs.next()) else {
            // Bare `module` / `module.<label>` — not a concrete output ref.
            return index_operators_apply_time(t, ctx, binds, visited);
        };
        let rest: Vec<&str> = get_attrs.collect();
        let verdict = ctx
            .module_outputs
            .and_then(|lookup| lookup.output_apply_time(label, output, &rest))
            .unwrap_or(false);
        return verdict || index_operators_apply_time(t, ctx, binds, visited);
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
    // An allowlisted plan-known computed collection with an explicit field
    // selection (e.g. `...domain_validation_options[*].domain_name`): the
    // provider populates the listed fields at plan time. A *bare* collection
    // reference stays apply-time here — its membership may be plan-known
    // (handled in the membership / length contexts) but its element values
    // are not.
    if let Some(AllowlistedUse {
        tail: Some(unknown),
        ..
    }) = allowlisted_traversal(t)
    {
        return unknown || index_operators_apply_time(t, ctx, binds, visited);
    }
    // Anything else is a managed-resource reference: `<type>.<name>.<attr>`.
    resource_reference_apply_time(t, head, ctx, binds, visited)
}

/// Whether a managed-resource reference `<type>.<name>.<attr>` is apply-time.
/// An attribute set explicitly in the resource's config takes the config
/// expression's value — plan-known iff that expression is. An attribute NOT
/// in config is computed (unknown while the resource has pending changes),
/// and a block we cannot resolve keeps the conservative default: flag.
///
/// `visited` carries `res:<type>.<name>` keys to break reference cycles
/// (shared with the `local.*` / `data:` keys — the shapes cannot collide).
/// A cycle keeps the conservative default.
fn resource_reference_apply_time(
    t: &hcl_edit::expr::Traversal,
    rtype: &str,
    ctx: &UnknownCtx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    let mut get_attrs = t.operators.iter().filter_map(|op| match op.value() {
        TraversalOperator::GetAttr(ident) => Some(ident.as_str()),
        _ => None,
    });
    let (Some(name), Some(attr)) = (get_attrs.next(), get_attrs.next()) else {
        // Whole-resource reference — the object carries computed attrs.
        return true;
    };
    let key = format!("res:{rtype}.{name}");
    if visited.contains(&key) {
        return true; // cycle — keep the conservative default
    }
    if let Some(expr) = ctx
        .resource_configs
        .get(&(rtype.to_string(), name.to_string()))
        .and_then(|config| config.attrs.get(attr))
    {
        if index_operators_apply_time(t, ctx, binds, visited) {
            return true;
        }
        let mut v2 = visited.clone();
        v2.insert(key);
        // Config expressions are evaluated in the resource block's own scope.
        let no_binds = Binds::new();
        return references_apply_time(expr, ctx, &no_binds, &v2);
    }
    // Not set in config (or block unseen): a *non-computed* attribute per the
    // provider schema can only hold a config value or null — never a
    // provider-populated unknown — so it is plan-known. Computed attributes,
    // unknown attributes, and absent schemas keep the conservative default.
    if let Some(lookup) = ctx.schema {
        if let Some(schema) = lookup.resource(rtype) {
            if let Some(attr_schema) = schema.block.attributes.get(attr) {
                if !attr_schema.computed {
                    return index_operators_apply_time(t, ctx, binds, visited);
                }
            }
        }
    }
    true
}

/// Whether a `data.<type>.<name>...` reference is apply-time: the data
/// source's read is deferred (and its attributes unknown) iff it has a
/// `depends_on` on a managed resource or any of its own config expressions
/// transitively references an apply-time value. A block we cannot resolve in
/// the module is treated as plan-known — prefer false negatives.
///
/// `visited` (shared with `local.*` resolution) carries `data:<type>.<name>`
/// keys to break reference cycles; the key shape cannot collide with local
/// names. A cycle resolves to plan-known (stay silent).
fn data_reference_apply_time(
    t: &hcl_edit::expr::Traversal,
    ctx: &UnknownCtx,
    binds: &Binds,
    visited: &HashSet<String>,
) -> bool {
    let mut get_attrs = t.operators.iter().filter_map(|op| match op.value() {
        TraversalOperator::GetAttr(ident) => Some(ident.as_str()),
        _ => None,
    });
    let (Some(dtype), Some(name)) = (get_attrs.next(), get_attrs.next()) else {
        // Bare `data` / `data.<type>` — not a concrete reference.
        return index_operators_apply_time(t, ctx, binds, visited);
    };
    if index_operators_apply_time(t, ctx, binds, visited) {
        return true;
    }
    let key = format!("data:{dtype}.{name}");
    if visited.contains(&key) {
        return false; // cycle — assume plan-known
    }
    let Some(config) = ctx
        .data_configs
        .get(&(dtype.to_string(), name.to_string()))
    else {
        return false; // unresolvable block — stay silent
    };
    if config.has_depends_on {
        return true;
    }
    let mut v2 = visited.clone();
    v2.insert(key);
    // Config expressions are evaluated in the data block's own scope — no
    // comprehension loop variables from the referencing context apply.
    let no_binds = Binds::new();
    config
        .attrs
        .values()
        .chain(config.nested_exprs.iter())
        .any(|e| references_apply_time(e, ctx, &no_binds, &v2))
}

fn index_operators_apply_time(
    t: &hcl_edit::expr::Traversal,
    ctx: &UnknownCtx,
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
    ctx: &UnknownCtx,
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
    ctx: &UnknownCtx,
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
        other => {
            // Allowlisted plan-known computed collection: element fields the
            // provider populates at plan are known; everything else isn't.
            if let Expression::Traversal(t) = other {
                if let Some(a) = allowlisted_traversal(t) {
                    if a.tail.is_none() {
                        return match field {
                            Some(f) => !a.known_fields.contains(&f),
                            // Whole-element use carries apply-time fields.
                            None => true,
                        };
                    }
                }
            }
            references_apply_time(other, ctx, binds, &v2)
        }
    }
}

fn element_field_apply_time(
    elem: &Expression,
    field: Option<&str>,
    ctx: &UnknownCtx,
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
    ctx: &UnknownCtx,
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
        other => {
            // Keys of a caller-passed collection: membership bit only.
            if let Some(name) = var_name(other) {
                return ctx
                    .unknown_variables
                    .get(name)
                    .is_some_and(|info| info.membership);
            }
            references_apply_time(other, ctx, binds, &v2)
        }
    }
}

/// Follow a chain of `local.*` references to the underlying definition,
/// stopping at the first non-local expression (or a cycle). Returns the
/// resolved expression and the `visited` set extended with every local name
/// traversed (so callers guard cycles when recursing into the result).
fn resolve_through_locals<'a>(
    expr: &'a Expression,
    ctx: &'a UnknownCtx<'a>,
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
    ctx: &UnknownCtx<'a>,
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
    head_name(expr, "local")
}

/// If `expr` is a bare `var.<name>` traversal (no operators beyond the name),
/// return `<name>`.
fn var_name(expr: &Expression) -> Option<&str> {
    let Expression::Traversal(t) = expr else {
        return None;
    };
    if t.operators.len() != 1 {
        return None;
    }
    head_name(expr, "var")
}

fn head_name<'e>(expr: &'e Expression, expected_head: &str) -> Option<&'e str> {
    let Expression::Traversal(t) = expr else {
        return None;
    };
    let Expression::Variable(head) = &t.expr else {
        return None;
    };
    if head.as_str() != expected_head {
        return None;
    }
    first_attr(t)
}

/// First `var.<name>` reference anywhere inside `expr` that has an
/// [`UnknownVarInfo`] entry — rule drivers use this to append the
/// caller-origin reason to a fired diagnostic's message.
pub fn unknown_var_reason<'a>(expr: &Expression, ctx: &UnknownCtx<'a>) -> Option<&'a str> {
    let mut found: Option<&'a str> = None;
    crate::expr_walk::for_each_expression_in(expr, |e| {
        if found.is_some() {
            return;
        }
        if let Expression::Traversal(t) = e {
            if let Expression::Variable(head) = &t.expr {
                if head.as_str() == "var" {
                    if let Some(info) = first_attr(t).and_then(|n| ctx.unknown_variables.get(n)) {
                        if info.membership || info.value {
                            found = Some(info.reason.as_str());
                        }
                    }
                }
            }
        }
    });
    found
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

pub(crate) fn expr_range(expr: &Expression, rope: &Rope) -> Range {
    expr.span()
        .and_then(|sp| hcl_span_to_lsp_range(rope, sp).ok())
        .unwrap_or_default()
}
