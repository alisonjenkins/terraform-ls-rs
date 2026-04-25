//! Structural types parsed from Terraform `variable "…" { type = … }`
//! declarations. Used to power attribute completion on `var.NAME.field…`.

use std::collections::BTreeMap;

use hcl_edit::expr::{Expression, Traversal, TraversalOperator};
use serde::{Deserialize, Serialize};

/// A Terraform variable type — the shape declared via `type = …` in a
/// `variable` block. Only `Object` carries drill-in information for
/// completion, but the other variants exist so we can faithfully
/// represent any legal type expression (useful for future hover/diag
/// features).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VariableType {
    Any,
    Primitive(Primitive),
    List(Box<VariableType>),
    Set(Box<VariableType>),
    Map(Box<VariableType>),
    Tuple(Vec<VariableType>),
    Object(BTreeMap<String, VariableType>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Primitive {
    String,
    Number,
    Bool,
}

impl std::fmt::Display for Primitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Primitive::String => f.write_str("string"),
            Primitive::Number => f.write_str("number"),
            Primitive::Bool => f.write_str("bool"),
        }
    }
}

impl std::fmt::Display for VariableType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VariableType::Any => f.write_str("any"),
            VariableType::Primitive(p) => write!(f, "{p}"),
            VariableType::List(inner) => write!(f, "list({inner})"),
            VariableType::Set(inner) => write!(f, "set({inner})"),
            VariableType::Map(inner) => write!(f, "map({inner})"),
            VariableType::Tuple(items) => {
                f.write_str("tuple([")?;
                for (i, t) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{t}")?;
                }
                f.write_str("])")
            }
            VariableType::Object(fields) => {
                f.write_str("object({ ")?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k} = {v}")?;
                }
                f.write_str(" })")
            }
        }
    }
}

/// Parse a Terraform type expression into a [`VariableType`].
///
/// Unknown or malformed expressions fall back to [`VariableType::Any`]
/// — documents are often mid-edit, so bailing hard would make the
/// feature unusable in practice.
pub fn parse_type_expr(expr: &Expression) -> VariableType {
    match expr {
        Expression::Variable(v) => match v.value().as_str() {
            "string" => VariableType::Primitive(Primitive::String),
            "number" => VariableType::Primitive(Primitive::Number),
            "bool" => VariableType::Primitive(Primitive::Bool),
            "any" => VariableType::Any,
            _ => VariableType::Any,
        },
        Expression::FuncCall(call) => {
            // Namespaced type functions aren't a thing in Terraform.
            if !call.name.namespace.is_empty() {
                return VariableType::Any;
            }
            let func_name = call.name.name.as_str();
            let args: Vec<&Expression> = call.args.iter().collect();
            match (func_name, args.as_slice()) {
                ("list", [inner]) => VariableType::List(Box::new(parse_type_expr(inner))),
                ("set", [inner]) => VariableType::Set(Box::new(parse_type_expr(inner))),
                ("map", [inner]) => VariableType::Map(Box::new(parse_type_expr(inner))),
                ("tuple", [Expression::Array(arr)]) => {
                    let items: Vec<VariableType> =
                        arr.iter().map(parse_type_expr).collect();
                    VariableType::Tuple(items)
                }
                ("object", [Expression::Object(obj)]) => {
                    let mut fields = BTreeMap::new();
                    for (key, value) in obj.iter() {
                        let Some(name) = object_key_as_ident(key) else {
                            continue;
                        };
                        fields.insert(name, parse_type_expr(value.expr()));
                    }
                    VariableType::Object(fields)
                }
                // `optional(T)` / `optional(T, default)` — mark the
                // field as allowed-to-be-missing by collapsing to Any.
                // This sacrifices the inner type info (present-value
                // type-checking no longer applies for this field) in
                // exchange for avoiding false "missing field" positives
                // on variables that use the optional wrapper.
                ("optional", [_]) | ("optional", [_, _]) => VariableType::Any,
                _ => VariableType::Any,
            }
        }
        _ => VariableType::Any,
    }
}

pub(crate) fn object_key_as_ident(key: &hcl_edit::expr::ObjectKey) -> Option<String> {
    match key {
        hcl_edit::expr::ObjectKey::Ident(ident) => Some(ident.as_str().to_string()),
        hcl_edit::expr::ObjectKey::Expression(Expression::Variable(v)) => {
            Some(v.value().as_str().to_string())
        }
        hcl_edit::expr::ObjectKey::Expression(Expression::String(s)) => {
            Some(s.value().to_string())
        }
        _ => None,
    }
}

/// Parse a *value-position* expression (literal, `toset(…)`, ternary,
/// object literal, array literal, etc.) into a [`VariableType`] capturing
/// what we can statically deduce about its shape — in particular, what
/// keys appear at each level so bracket/dot completion can enumerate
/// them. Unknown expressions degrade to [`VariableType::Any`].
/// Optional schema oracle used by [`parse_value_shape_with_schema`]
/// to resolve resource / data-source attribute traversals against
/// real provider schemas. Implemented by `tfls-state::StateStore` so
/// the inference path picks up the loaded schemas at runtime; the
/// schema-free [`parse_value_shape`] is kept for callers (tests,
/// `tfls-diag` validation rules) that don't need the lookup.
pub trait SchemaLookup {
    /// `<resource_type>.<name>.<attr>` → resolved type. `None` when
    /// the resource type isn't loaded or the attribute doesn't
    /// exist on the schema.
    fn resource_attr(&self, resource_type: &str, attr: &str) -> Option<VariableType>;
    /// Same as `resource_attr` but for `data.<type>.<name>.<attr>`.
    fn data_source_attr(&self, type_name: &str, attr: &str) -> Option<VariableType>;
    /// `var.<name>` in the current scope. Default `None` keeps
    /// callers that don't have local variable context working.
    fn variable_type(&self, _name: &str) -> Option<VariableType> {
        None
    }
    /// `local.<name>` in the current scope. Default `None`.
    fn local_shape(&self, _name: &str) -> Option<VariableType> {
        None
    }
    /// `module.<name>.<output>` — the named module's declared
    /// output type. Default `None`.
    fn module_output(&self, _module: &str, _output: &str) -> Option<VariableType> {
        None
    }
    /// Type of `each.value` when resolving an expression inside a
    /// `for_each = …` block. Default `None` for callers outside
    /// for_each scope.
    fn each_value(&self) -> Option<VariableType> {
        None
    }
}

/// Schema-free convenience wrapper. Equivalent to calling
/// [`parse_value_shape_with_schema`] with a no-op lookup that always
/// returns `None`.
pub fn parse_value_shape(expr: &Expression) -> VariableType {
    parse_value_shape_with_schema(expr, &NoSchemaLookup)
}

struct NoSchemaLookup;
impl SchemaLookup for NoSchemaLookup {
    fn resource_attr(&self, _: &str, _: &str) -> Option<VariableType> {
        None
    }
    fn data_source_attr(&self, _: &str, _: &str) -> Option<VariableType> {
        None
    }
}

/// Schema-aware version: a `Traversal` that names a resource or data
/// source attribute resolves to its provider-schema type, and a
/// for-expression iterating a known resource/data source resolves
/// its body's attribute access through the same lookup.
pub fn parse_value_shape_with_schema(
    expr: &Expression,
    schema: &dyn SchemaLookup,
) -> VariableType {
    match expr {
        Expression::Object(obj) => {
            let mut fields = BTreeMap::new();
            for (key, value) in obj.iter() {
                let Some(name) = object_key_as_ident(key) else {
                    continue;
                };
                fields.insert(name, parse_value_shape_with_schema(value.expr(), schema));
            }
            VariableType::Object(fields)
        }
        Expression::Array(arr) => {
            let items: Vec<VariableType> = arr
                .iter()
                .map(|e| parse_value_shape_with_schema(e, schema))
                .collect();
            VariableType::Tuple(items)
        }
        Expression::String(_) => VariableType::Primitive(Primitive::String),
        // String interpolations always evaluate to a string. The
        // result of `"${anything}"` and heredocs is `string` per
        // Terraform's type system, regardless of the inner
        // expressions.
        Expression::StringTemplate(_) | Expression::HeredocTemplate(_) => {
            VariableType::Primitive(Primitive::String)
        }
        Expression::Number(_) => VariableType::Primitive(Primitive::Number),
        Expression::Bool(_) => VariableType::Primitive(Primitive::Bool),
        Expression::FuncCall(call) => {
            if !call.name.namespace.is_empty() {
                return VariableType::Any;
            }
            match call.name.name.as_str() {
                "toset" | "tolist" => {
                    if let Some(Expression::Array(arr)) = call.args.iter().next() {
                        let mut keys: BTreeMap<String, VariableType> = BTreeMap::new();
                        for item in arr.iter() {
                            if let Expression::String(s) = item {
                                keys.insert(s.value().to_string(), VariableType::Any);
                            }
                        }
                        if !keys.is_empty() {
                            return VariableType::Object(keys);
                        }
                    }
                    VariableType::Any
                }
                "tomap" => call
                    .args
                    .iter()
                    .next()
                    .map(|e| parse_value_shape_with_schema(e, schema))
                    .unwrap_or(VariableType::Any),
                // `element(list, idx)` returns the element type of
                // its first argument. For inference, walk the first
                // arg recursively: `Tuple([T, ...])` → T, `List(T)`
                // → T, anything else → Any.
                "element" => {
                    let Some(first) = call.args.iter().next() else {
                        return VariableType::Any;
                    };
                    let collected = parse_value_shape_with_schema(first, schema);
                    match collected {
                        VariableType::Tuple(items) => {
                            if items.is_empty() {
                                VariableType::Any
                            } else if items.iter().all(|t| t == &items[0]) {
                                items.into_iter().next().unwrap_or(VariableType::Any)
                            } else {
                                VariableType::Any
                            }
                        }
                        VariableType::List(inner) | VariableType::Set(inner) => *inner,
                        _ => VariableType::Any,
                    }
                }
                // `concat(list1, list2, ...)` returns a list of the
                // common element type. If every arg yields a
                // homogeneous element type we keep it; otherwise
                // fall back to Any.
                "concat" => {
                    let mut element_types: Vec<VariableType> = Vec::new();
                    for arg in call.args.iter() {
                        match parse_value_shape_with_schema(arg, schema) {
                            VariableType::Tuple(items) => element_types.extend(items),
                            VariableType::List(inner) | VariableType::Set(inner) => {
                                element_types.push(*inner)
                            }
                            other => element_types.push(other),
                        }
                    }
                    if element_types.is_empty() {
                        VariableType::Any
                    } else {
                        let first = element_types[0].clone();
                        if element_types.iter().all(|t| t == &first) {
                            VariableType::List(Box::new(first))
                        } else {
                            VariableType::Any
                        }
                    }
                }
                // Common string-returning functions.
                "format" | "join" | "trim" | "trimspace" | "trimprefix" | "trimsuffix"
                | "upper" | "lower" | "title" | "replace" | "regex" | "abspath"
                | "dirname" | "basename" | "pathexpand" | "uuid" | "uuidv5"
                | "base64encode" | "base64decode" | "filebase64" | "filemd5"
                | "filesha1" | "filesha256" | "filesha512" | "md5" | "sha1"
                | "sha256" | "sha512" | "bcrypt" | "templatefile" | "file"
                | "yamlencode" | "jsonencode" | "timestamp" | "formatdate" => {
                    VariableType::Primitive(Primitive::String)
                }
                "length" | "ceil" | "floor" | "abs" | "log" | "max" | "min"
                | "pow" | "signum" | "parseint" => VariableType::Primitive(Primitive::Number),
                "alltrue" | "anytrue" | "can" | "contains" | "startswith"
                | "endswith" | "fileexists" | "issensitive" => {
                    VariableType::Primitive(Primitive::Bool)
                }
                "split" => VariableType::List(Box::new(VariableType::Primitive(Primitive::String))),
                "keys" => VariableType::List(Box::new(VariableType::Primitive(Primitive::String))),
                "values" => {
                    // `values(map)` returns a list of the map's element type.
                    if let Some(first) = call.args.iter().next() {
                        match parse_value_shape_with_schema(first, schema) {
                            VariableType::Map(inner) => return VariableType::List(inner),
                            VariableType::Object(fields) => {
                                let mut iter = fields.values();
                                if let Some(first_val) = iter.next() {
                                    let f = first_val.clone();
                                    if iter.all(|v| v == &f) {
                                        return VariableType::List(Box::new(f));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    VariableType::Any
                }
                "merge" => {
                    // `merge(obj1, obj2, ...)` — combine all object
                    // fields. Keys from later args win.
                    let mut combined = BTreeMap::new();
                    let mut all_objects = true;
                    for arg in call.args.iter() {
                        match parse_value_shape_with_schema(arg, schema) {
                            VariableType::Object(fields) => combined.extend(fields),
                            _ => {
                                all_objects = false;
                                break;
                            }
                        }
                    }
                    if all_objects && !combined.is_empty() {
                        VariableType::Object(combined)
                    } else {
                        VariableType::Any
                    }
                }
                "lookup" => {
                    // `lookup(map, key, default?)` returns the map's
                    // value type. Pull from first arg if it's a map.
                    // If the map's value type isn't homogeneous (or
                    // the first arg isn't a map at all — e.g.
                    // `each.value` from a heterogeneous for_each),
                    // fall back to the third arg's type. Common
                    // pattern in user code:
                    //
                    //     auth_lambda_edge_arn = lookup(
                    //         each.value, "auth_lambda_edge_arn", null)
                    //     spa_mode = lookup(each.value, "spa_mode", false)
                    //
                    // The third arg pins the expected type when
                    // the dict is heterogeneous.
                    if let Some(first) = call.args.iter().next() {
                        match parse_value_shape_with_schema(first, schema) {
                            VariableType::Map(inner) => return *inner,
                            VariableType::Object(fields) => {
                                let mut iter = fields.values();
                                if let Some(first_val) = iter.next() {
                                    let f = first_val.clone();
                                    if iter.all(|v| v == &f) {
                                        return f;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    if let Some(default) = call.args.iter().nth(2) {
                        let t = parse_value_shape_with_schema(default, schema);
                        if !matches!(&t, VariableType::Any) {
                            return t;
                        }
                    }
                    VariableType::Any
                }
                "try" => {
                    // `try(a, b, c)` returns the type of the first
                    // arg that doesn't error. We can't simulate
                    // failure, so pick the first that yields
                    // something concrete.
                    for arg in call.args.iter() {
                        let t = parse_value_shape_with_schema(arg, schema);
                        if !matches!(&t, VariableType::Any) {
                            return t;
                        }
                    }
                    VariableType::Any
                }
                "coalesce" => {
                    // `coalesce(a, b, c)` returns the first non-null.
                    // Take the first concrete arg's type.
                    for arg in call.args.iter() {
                        let t = parse_value_shape_with_schema(arg, schema);
                        if !matches!(&t, VariableType::Any) {
                            return t;
                        }
                    }
                    VariableType::Any
                }
                // Users sometimes write `object({ … })` in a value
                // position by mistake (confusing it with the type
                // expression). Treat it as the object literal the
                // user likely meant, so the type-mismatch diagnostic
                // has something concrete to compare against.
                "object" => {
                    if let Some(Expression::Object(obj)) = call.args.iter().next() {
                        let mut fields = BTreeMap::new();
                        for (key, value) in obj.iter() {
                            let Some(name) = object_key_as_ident(key) else {
                                continue;
                            };
                            fields.insert(
                                name,
                                parse_value_shape_with_schema(value.expr(), schema),
                            );
                        }
                        return VariableType::Object(fields);
                    }
                    VariableType::Any
                }
                _ => VariableType::Any,
            }
        }
        Expression::Conditional(c) => merge_shapes(
            parse_value_shape_with_schema(&c.true_expr, schema),
            parse_value_shape_with_schema(&c.false_expr, schema),
        ),
        Expression::Parenthesis(inner) => parse_value_shape_with_schema(inner.inner(), schema),
        Expression::Traversal(tv) => {
            // `aws_subnet.foo.id` / `data.aws_subnet.foo.id` →
            // schema lookup. Anything else (var.x, local.x,
            // module.x) collapses to `Any` — runtime values we
            // can't statically classify.
            traversal_attr_type(tv, schema).unwrap_or(VariableType::Any)
        }
        Expression::ForExpr(f) => {
            // `[for x in <resource>.<name> : x.<attr>]` resolves
            // through the schema oracle when both the collection
            // is a known resource/data source and the body
            // accesses one of its attributes via the iterator
            // binding. Otherwise we keep the previous best-effort
            // behaviour and fall back to `list(any)` / `map(any)`,
            // which still renders as valid HCL the user can refine.
            let iter_var = f.intro.value_var.as_str();
            let elem_via_schema = for_expr_body_via_schema(
                &f.intro.collection_expr,
                iter_var,
                &f.value_expr,
                schema,
            );
            let elem = elem_via_schema
                .unwrap_or_else(|| parse_value_shape_with_schema(&f.value_expr, schema));
            if f.key_expr.is_some() {
                VariableType::Map(Box::new(elem))
            } else {
                VariableType::List(Box::new(elem))
            }
        }
        _ => VariableType::Any,
    }
}

/// Resolve `<resource_type>.<name>.<attr>` /
/// `data.<resource_type>.<name>.<attr>` traversals against the
/// schema oracle. Also handles `var.<name>` and `local.<name>` via
/// the optional caller-scope methods on [`SchemaLookup`].
fn traversal_attr_type(tv: &Traversal, schema: &dyn SchemaLookup) -> Option<VariableType> {
    let base = match &tv.expr {
        Expression::Variable(v) => v.as_str().to_string(),
        _ => return None,
    };
    // Collect `.ident` operators, skipping `[…]` indexes / splats —
    // a `for_each` / `count` instance access (`module.X[k].out`,
    // `aws_subnet.foo[0].id`) is invariant in the type we care about,
    // so an Index in the middle of the chain shouldn't truncate the
    // attribute path.
    let mut idents: Vec<&str> = Vec::new();
    for op in &tv.operators {
        match op.value() {
            TraversalOperator::GetAttr(i) => idents.push(i.as_str()),
            TraversalOperator::Index(_)
            | TraversalOperator::LegacyIndex(_)
            | TraversalOperator::AttrSplat(_)
            | TraversalOperator::FullSplat(_) => continue,
        }
    }
    if base == "var" {
        // `var.<name>` → look up the caller's declared variable type.
        let name = *idents.first()?;
        let ty = schema.variable_type(name)?;
        return Some(drill_into_object(ty, &idents[1..]));
    }
    if base == "local" {
        let name = *idents.first()?;
        let ty = schema.local_shape(name)?;
        return Some(drill_into_object(ty, &idents[1..]));
    }
    if base == "module" {
        // `module.<name>.<output>` → child module's output type.
        let mod_name = *idents.first()?;
        let output = *idents.get(1)?;
        let ty = schema.module_output(mod_name, output)?;
        return Some(drill_into_object(ty, &idents[2..]));
    }
    if base == "each" {
        // `each.key` is always string in `for_each` scope.
        // `each.value[.<field>...]` resolves through the for_each
        // collection's element type.
        let what = *idents.first()?;
        if what == "key" {
            return Some(VariableType::Primitive(Primitive::String));
        }
        if what == "value" {
            let ty = schema.each_value()?;
            return Some(drill_into_object(ty, &idents[1..]));
        }
        return None;
    }
    if base == "data" {
        // data.<type>.<name>.<attr…> — attr is idents[2].
        let resource_type = *idents.first()?;
        let _name = idents.get(1)?;
        let attr = *idents.get(2)?;
        return schema.data_source_attr(resource_type, attr);
    }
    if !is_resource_type(&base) {
        return None;
    }
    // <type>.<name>.<attr…> — attr is idents[1].
    let _name = idents.first()?;
    let attr = *idents.get(1)?;
    schema.resource_attr(&base, attr)
}

/// Recognise `[for v in <resource>.<name> : v.<attr>]` (or the
/// data-source equivalent) and resolve `<attr>`'s type from the
/// schema. Returns `None` when the shape doesn't match — the
/// caller falls back to plain inference (likely `Any`).
fn for_expr_body_via_schema(
    collection_expr: &Expression,
    iter_var: &str,
    body: &Expression,
    schema: &dyn SchemaLookup,
) -> Option<VariableType> {
    let body_tv = match body {
        Expression::Traversal(tv) => tv,
        _ => return None,
    };
    let body_base = match &body_tv.expr {
        Expression::Variable(v) => v.as_str().to_string(),
        _ => return None,
    };
    if body_base != iter_var {
        return None;
    }
    let body_attr = body_tv
        .operators
        .first()
        .and_then(|op| match op.value() {
            TraversalOperator::GetAttr(i) => Some(i.as_str()),
            _ => None,
        })?;

    let coll_tv = match collection_expr {
        Expression::Traversal(tv) => tv,
        _ => return None,
    };
    let coll_base = match &coll_tv.expr {
        Expression::Variable(v) => v.as_str().to_string(),
        _ => return None,
    };
    let mut coll_idents: Vec<&str> = Vec::new();
    for op in &coll_tv.operators {
        match op.value() {
            TraversalOperator::GetAttr(i) => coll_idents.push(i.as_str()),
            _ => break,
        }
    }
    if coll_base == "data" {
        let resource_type = *coll_idents.first()?;
        let _name = coll_idents.get(1)?;
        return schema.data_source_attr(resource_type, body_attr);
    }
    if !is_resource_type(&coll_base) {
        return None;
    }
    let _name = coll_idents.first()?;
    schema.resource_attr(&coll_base, body_attr)
}

fn is_resource_type(s: &str) -> bool {
    s.contains('_')
}

/// Walk a chain of `.field` accessors against an inferred value
/// shape, descending one level per ident. `Object` lookups return
/// the named field; `Map(T)`/`List(T)`/`Set(T)` return their
/// element type for the next step. Anything else collapses to
/// `Any`. Empty `path` returns the input unchanged — used as the
/// no-op for traversals that already resolved to their target.
pub(crate) fn drill_into_object(mut ty: VariableType, path: &[&str]) -> VariableType {
    for ident in path {
        ty = match ty {
            VariableType::Object(fields) => fields
                .get(*ident)
                .cloned()
                .unwrap_or(VariableType::Any),
            VariableType::Map(inner) | VariableType::List(inner) | VariableType::Set(inner) => {
                *inner
            }
            _ => VariableType::Any,
        };
    }
    ty
}

/// Check whether a value whose inferred shape is `actual` satisfies
/// the declared `type` constraint. `Any` on either side is a free
/// pass (we can't statically know), so this is intentionally
/// conservative — the goal is to catch clear authoring mistakes like
/// `type = string` + `default = {}`, not to re-implement Terraform's
/// full type system.
///
/// Semantics:
/// - Primitive mismatches → `false` (e.g. `string` vs `number`)
/// - Primitive vs collection (or vice versa) → `false`
/// - Collection inner types: recurse where we can. Array literals
///   parse as `Tuple`; `list(T)` / `set(T)` accept a `Tuple` if every
///   element satisfies `T`.
/// - `map(T)` accepts an object literal if every value satisfies `T`.
/// - Object vs object: the declared schema is authoritative. Extra
///   fields in the actual value fail. Declared fields typed as
///   anything other than `Any` must be present — `optional(…)`
///   fields parse as `Any` (see [`parse_type_expr`]) and are thus
///   allowed to be absent without a false positive.
pub fn satisfies(declared: &VariableType, actual: &VariableType) -> bool {
    use VariableType::*;
    match (declared, actual) {
        (Any, _) | (_, Any) => true,
        (Primitive(a), Primitive(b)) => a == b,
        (List(inner), Tuple(items)) | (Set(inner), Tuple(items)) => {
            items.iter().all(|it| satisfies(inner, it))
        }
        (List(_), List(_))
        | (List(_), Set(_))
        | (Set(_), Set(_))
        | (Set(_), List(_))
        | (Map(_), Map(_)) => true,
        (Map(inner), Object(fields)) => fields.values().all(|v| satisfies(inner, v)),
        (Tuple(decl_items), Tuple(act_items)) => {
            decl_items.len() == act_items.len()
                && decl_items
                    .iter()
                    .zip(act_items.iter())
                    .all(|(d, a)| satisfies(d, a))
        }
        (Object(decl), Object(act)) => {
            // Extra fields in the actual value that the declared
            // schema doesn't know about — fail.
            if act.keys().any(|k| !decl.contains_key(k)) {
                return false;
            }
            // Declared fields must either be present with a matching
            // type, or — if declared as `Any` (our stand-in for
            // `optional(T)` or a type expression we couldn't parse) —
            // permitted to be absent.
            decl.iter().all(|(k, d_ty)| match act.get(k) {
                Some(a_ty) => satisfies(d_ty, a_ty),
                None => matches!(d_ty, Any),
            })
        }
        _ => false,
    }
}

/// Produce a human-readable explanation of why `actual` doesn't
/// satisfy `declared`. Returns an empty string if they match.
/// Rendered into the diagnostic message so the user sees *which*
/// field is wrong, not just "mismatch".
pub fn explain_mismatch(declared: &VariableType, actual: &VariableType) -> String {
    use VariableType::*;
    if satisfies(declared, actual) {
        return String::new();
    }
    match (declared, actual) {
        (Object(decl), Object(act)) => {
            let extras: Vec<_> = act
                .keys()
                .filter(|k| !decl.contains_key(k.as_str()))
                .cloned()
                .collect();
            let missing: Vec<_> = decl
                .iter()
                .filter(|(k, d_ty)| !act.contains_key(k.as_str()) && !matches!(d_ty, Any))
                .map(|(k, _)| k.clone())
                .collect();
            let wrong: Vec<_> = decl
                .iter()
                .filter_map(|(k, d_ty)| {
                    act.get(k).and_then(|a_ty| {
                        (!satisfies(d_ty, a_ty)).then(|| {
                            format!("`{k}` expected `{d_ty}`, got `{a_ty}`")
                        })
                    })
                })
                .collect();
            let mut parts = Vec::new();
            if !extras.is_empty() {
                parts.push(format!(
                    "unknown field{s}: {fields}",
                    s = if extras.len() == 1 { "" } else { "s" },
                    fields = extras
                        .iter()
                        .map(|k| format!("`{k}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                ));
            }
            if !missing.is_empty() {
                parts.push(format!(
                    "missing field{s}: {fields}",
                    s = if missing.len() == 1 { "" } else { "s" },
                    fields = missing
                        .iter()
                        .map(|k| format!("`{k}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                ));
            }
            parts.extend(wrong);
            parts.join("; ")
        }
        _ => format!("expected `{declared}`, got `{actual}`"),
    }
}

/// Combine two inferred shapes: the more informative wins; two
/// [`VariableType::Object`] shapes union their keys and recursively
/// merge overlapping values.
pub fn merge_shapes(a: VariableType, b: VariableType) -> VariableType {
    match (a, b) {
        (VariableType::Any, other) | (other, VariableType::Any) => other,
        (VariableType::Object(mut a_fields), VariableType::Object(b_fields)) => {
            for (k, v) in b_fields {
                let merged = match a_fields.remove(&k) {
                    Some(existing) => merge_shapes(existing, v),
                    None => v,
                };
                a_fields.insert(k, merged);
            }
            VariableType::Object(a_fields)
        }
        (a, _) => a,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use hcl_edit::structure::{Attribute, Body};

    fn type_from_src(src: &str) -> VariableType {
        let body: Body = src.parse().expect("parses");
        for structure in body.iter() {
            let Some(attr): Option<&Attribute> = structure.as_attribute() else {
                continue;
            };
            if attr.key.as_str() == "type" {
                return parse_type_expr(&attr.value);
            }
        }
        panic!("no `type = …` attribute in source: {src:?}")
    }

    fn shape_from_src(src: &str) -> VariableType {
        let body: Body = src.parse().expect("parses");
        for structure in body.iter() {
            let Some(attr): Option<&Attribute> = structure.as_attribute() else {
                continue;
            };
            if attr.key.as_str() == "value" {
                return parse_value_shape(&attr.value);
            }
        }
        panic!("no `value = …` attribute in source: {src:?}")
    }

    #[test]
    fn parse_primitive_string() {
        assert_eq!(
            type_from_src("type = string"),
            VariableType::Primitive(Primitive::String)
        );
    }

    #[test]
    fn parse_primitive_number() {
        assert_eq!(
            type_from_src("type = number"),
            VariableType::Primitive(Primitive::Number)
        );
    }

    #[test]
    fn parse_primitive_bool() {
        assert_eq!(
            type_from_src("type = bool"),
            VariableType::Primitive(Primitive::Bool)
        );
    }

    #[test]
    fn parse_any_falls_back() {
        assert_eq!(type_from_src("type = any"), VariableType::Any);
    }

    #[test]
    fn parse_list_of_string() {
        assert_eq!(
            type_from_src("type = list(string)"),
            VariableType::List(Box::new(VariableType::Primitive(Primitive::String)))
        );
    }

    #[test]
    fn parse_map_of_number() {
        assert_eq!(
            type_from_src("type = map(number)"),
            VariableType::Map(Box::new(VariableType::Primitive(Primitive::Number)))
        );
    }

    #[test]
    fn parse_object_with_fields() {
        let ty = type_from_src("type = object({ name = string, age = number })");
        match ty {
            VariableType::Object(fields) => {
                assert_eq!(
                    fields.get("name"),
                    Some(&VariableType::Primitive(Primitive::String))
                );
                assert_eq!(
                    fields.get("age"),
                    Some(&VariableType::Primitive(Primitive::Number))
                );
            }
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn parse_nested_object() {
        let ty = type_from_src("type = object({ inner = object({ x = bool }) })");
        match ty {
            VariableType::Object(fields) => match fields.get("inner") {
                Some(VariableType::Object(inner)) => {
                    assert_eq!(
                        inner.get("x"),
                        Some(&VariableType::Primitive(Primitive::Bool))
                    );
                }
                other => panic!("expected inner object, got {other:?}"),
            },
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn parse_tuple() {
        let ty = type_from_src("type = tuple([string, number, bool])");
        match ty {
            VariableType::Tuple(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], VariableType::Primitive(Primitive::String));
            }
            other => panic!("expected tuple, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_falls_back_to_any() {
        assert_eq!(type_from_src("type = 42"), VariableType::Any);
        assert_eq!(type_from_src("type = foo.bar"), VariableType::Any);
    }

    // --- parse_value_shape regressions ------------------------------

    #[test]
    fn value_shape_object_literal() {
        let ty = shape_from_src(r#"value = { "a" = 1, "b" = 2 }"#);
        match ty {
            VariableType::Object(fields) => {
                assert_eq!(
                    fields.get("a"),
                    Some(&VariableType::Primitive(Primitive::Number))
                );
                assert_eq!(
                    fields.get("b"),
                    Some(&VariableType::Primitive(Primitive::Number))
                );
            }
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn value_shape_toset_becomes_object_keys_any_values() {
        let ty = shape_from_src("value = toset([\"vpc\", \"dev\"])");
        match ty {
            VariableType::Object(fields) => {
                assert!(fields.contains_key("vpc"));
                assert!(fields.contains_key("dev"));
            }
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn value_shape_tomap_passthrough() {
        let ty = shape_from_src(r#"value = tomap({ "a" = 1 })"#);
        match ty {
            VariableType::Object(fields) => assert!(fields.contains_key("a")),
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn value_shape_conditional_unions_keys() {
        let ty = shape_from_src("value = true ? toset([\"a\"]) : toset([\"b\"])");
        match ty {
            VariableType::Object(fields) => {
                assert!(fields.contains_key("a"));
                assert!(fields.contains_key("b"));
            }
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn value_shape_nested_object_preserves_tree() {
        let ty = shape_from_src(
            r#"value = { "outer" = { "inner" = { "leaf" = true } } }"#,
        );
        match ty {
            VariableType::Object(top) => match top.get("outer") {
                Some(VariableType::Object(mid)) => match mid.get("inner") {
                    Some(VariableType::Object(leaf)) => {
                        assert!(leaf.contains_key("leaf"));
                    }
                    other => panic!("expected inner object, got {other:?}"),
                },
                other => panic!("expected outer object, got {other:?}"),
            },
            other => panic!("expected top object, got {other:?}"),
        }
    }

    #[test]
    fn value_shape_unknown_yields_any() {
        let ty = shape_from_src("value = var.x");
        assert_eq!(ty, VariableType::Any);
    }

    #[test]
    fn merge_shapes_unions_object_fields() {
        let a = VariableType::Object(BTreeMap::from([
            ("a".to_string(), VariableType::Any),
        ]));
        let b = VariableType::Object(BTreeMap::from([
            ("b".to_string(), VariableType::Primitive(Primitive::String)),
        ]));
        match merge_shapes(a, b) {
            VariableType::Object(fields) => {
                assert!(fields.contains_key("a"));
                assert!(fields.contains_key("b"));
            }
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn merge_shapes_any_is_identity() {
        let a = VariableType::Primitive(Primitive::Bool);
        assert_eq!(merge_shapes(VariableType::Any, a.clone()), a);
    }

    // --- satisfies regressions --------------------------------------

    fn decl(src: &str) -> VariableType {
        type_from_src(&format!("type = {src}"))
    }

    fn shape(src: &str) -> VariableType {
        shape_from_src(&format!("value = {src}"))
    }

    #[test]
    fn satisfies_any_is_free_pass() {
        assert!(satisfies(&VariableType::Any, &shape("{}")));
        assert!(satisfies(&decl("string"), &VariableType::Any));
    }

    #[test]
    fn satisfies_matching_primitives() {
        assert!(satisfies(&decl("string"), &shape(r#""hi""#)));
        assert!(satisfies(&decl("number"), &shape("42")));
        assert!(satisfies(&decl("bool"), &shape("true")));
    }

    #[test]
    fn satisfies_primitive_vs_collection_is_mismatch() {
        assert!(!satisfies(&decl("string"), &shape("{}")));
        assert!(!satisfies(&decl("string"), &shape("[1, 2]")));
        assert!(!satisfies(&decl("number"), &shape(r#"{ a = 1 }"#)));
    }

    #[test]
    fn satisfies_wrong_primitive_is_mismatch() {
        assert!(!satisfies(&decl("string"), &shape("42")));
        assert!(!satisfies(&decl("number"), &shape(r#""hi""#)));
        assert!(!satisfies(&decl("bool"), &shape(r#""true""#)));
    }

    #[test]
    fn satisfies_list_of_string_accepts_string_array() {
        assert!(satisfies(&decl("list(string)"), &shape(r#"["a", "b"]"#)));
        assert!(!satisfies(&decl("list(string)"), &shape("[1, 2]")));
    }

    #[test]
    fn satisfies_set_of_number_accepts_numeric_array() {
        assert!(satisfies(&decl("set(number)"), &shape("[1, 2]")));
        assert!(!satisfies(&decl("set(number)"), &shape(r#"["a"]"#)));
    }

    #[test]
    fn satisfies_map_of_string_checks_values() {
        assert!(satisfies(
            &decl("map(string)"),
            &shape(r#"{ a = "x", b = "y" }"#)
        ));
        assert!(!satisfies(
            &decl("map(string)"),
            &shape(r#"{ a = "x", b = 1 }"#)
        ));
    }

    #[test]
    fn satisfies_object_all_fields_present_with_matching_types() {
        assert!(satisfies(
            &decl("object({ a = string, b = number })"),
            &shape(r#"{ a = "x", b = 1 }"#)
        ));
        assert!(!satisfies(
            &decl("object({ a = string })"),
            &shape(r#"{ a = 1 }"#)
        ));
    }

    #[test]
    fn satisfies_object_missing_declared_field_flagged() {
        // A strictly-typed field (non-optional, known type) missing
        // from the actual value is a real schema violation.
        assert!(!satisfies(
            &decl("object({ a = string, b = number })"),
            &shape(r#"{ a = "x" }"#)
        ));
    }

    #[test]
    fn satisfies_object_missing_optional_field_allowed() {
        // optional(T) collapses to Any in parse_type_expr, which
        // permits absence — the common case for missing fields.
        assert!(satisfies(
            &decl("object({ a = string, b = optional(number) })"),
            &shape(r#"{ a = "x" }"#)
        ));
    }

    #[test]
    fn satisfies_nested_object_field_type_mismatch() {
        // Regression: the inner `age` value is an object literal, but
        // the declared type says `number` — must be flagged even
        // though the outer object fields otherwise line up.
        assert!(!satisfies(
            &decl("object({ name = string, age = number })"),
            &shape(r#"{ name = "Alison", age = { years = 38 } }"#)
        ));
    }

    #[test]
    fn value_shape_recognizes_object_function_call() {
        // `object({...})` as a value is technically a misuse
        // (Terraform reserves it for type expressions), but users do
        // write it. Treat it as the object literal they meant so the
        // mismatch diagnostic can catch the surrounding confusion.
        let ty = shape_from_src("value = object({ years = 38, months = true })");
        match ty {
            VariableType::Object(fields) => {
                assert_eq!(
                    fields.get("years"),
                    Some(&VariableType::Primitive(Primitive::Number))
                );
                assert_eq!(
                    fields.get("months"),
                    Some(&VariableType::Primitive(Primitive::Bool))
                );
            }
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn satisfies_object_extra_field_flagged() {
        assert!(!satisfies(
            &decl("object({ name = string })"),
            &shape(r#"{ a = "b" }"#)
        ));
    }

    #[test]
    fn display_renders_concise_form() {
        let ty = type_from_src("type = object({ name = string, inner = object({ x = bool }) })");
        let rendered = format!("{ty}");
        // BTreeMap sorts keys alphabetically: "inner" < "name".
        assert_eq!(rendered, "object({ inner = object({ x = bool }), name = string })");
    }

    // --- For-expression inference ---------------------------------
    //
    // `[for x in coll : x.id]` produces a list whose element type
    // tracks the body expression. When the body resolves to `Any`
    // (a traversal of a runtime collection — usual case for module
    // call sites), we still surface `list(any)` so the
    // type-inference quick-fix has something concrete to suggest.

    #[test]
    fn list_for_expr_with_traversal_body_is_list_any() {
        let ty = shape_from_src("value = [for s in aws_subnet.foo : s.id]");
        assert_eq!(ty, VariableType::List(Box::new(VariableType::Any)));
    }

    #[test]
    fn list_for_expr_with_string_body_is_list_string() {
        let ty = shape_from_src("value = [for k in [\"a\", \"b\"] : \"prefix-\"]");
        // Body is a string literal — element type is string.
        assert_eq!(
            ty,
            VariableType::List(Box::new(VariableType::Primitive(Primitive::String)))
        );
    }

    #[test]
    fn map_for_expr_is_map_with_value_type() {
        let ty = shape_from_src("value = {for k, v in {} : k => 1}");
        assert_eq!(
            ty,
            VariableType::Map(Box::new(VariableType::Primitive(Primitive::Number)))
        );
    }

    #[test]
    fn list_for_expr_renders_as_list_any() {
        let ty = shape_from_src("value = [for s in x : s.id]");
        assert_eq!(format!("{ty}"), "list(any)");
    }

    /// In-process fake `SchemaLookup` that returns canned types for
    /// `aws_subnet`'s `id` and `cidr_block` attributes — enough to
    /// pin the schema-aware inference path without dragging in
    /// `tfls-state` from a tfls-core test.
    struct FakeSchema;
    impl SchemaLookup for FakeSchema {
        fn resource_attr(&self, resource_type: &str, attr: &str) -> Option<VariableType> {
            match (resource_type, attr) {
                ("aws_subnet", "id") => Some(VariableType::Primitive(Primitive::String)),
                ("aws_subnet", "cidr_block") => Some(VariableType::Primitive(Primitive::String)),
                _ => None,
            }
        }
        fn data_source_attr(&self, type_name: &str, attr: &str) -> Option<VariableType> {
            match (type_name, attr) {
                ("aws_ami", "id") => Some(VariableType::Primitive(Primitive::String)),
                _ => None,
            }
        }
    }

    fn shape_with_schema(src: &str, schema: &dyn SchemaLookup) -> VariableType {
        use hcl_edit::structure::{Attribute, Body};
        let body: Body = src.parse().expect("parses");
        for structure in body.iter() {
            let Some(attr): Option<&Attribute> = structure.as_attribute() else {
                continue;
            };
            if attr.key.as_str() == "value" {
                return parse_value_shape_with_schema(&attr.value, schema);
            }
        }
        panic!("no `value = …` attribute in source: {src:?}")
    }

    #[test]
    fn schema_aware_traversal_resolves_resource_attribute_to_string() {
        // `aws_subnet.foo.id` → `string` per provider schema.
        let ty = shape_with_schema("value = aws_subnet.foo.id", &FakeSchema);
        assert_eq!(ty, VariableType::Primitive(Primitive::String));
    }

    #[test]
    fn schema_aware_traversal_resolves_data_source_attribute() {
        let ty = shape_with_schema("value = data.aws_ami.ubuntu.id", &FakeSchema);
        assert_eq!(ty, VariableType::Primitive(Primitive::String));
    }

    #[test]
    fn schema_aware_for_expr_over_resource_yields_list_of_attribute_type() {
        // `[for s in aws_subnet.foo : s.id]` → `list(string)` once
        // the schema knows `aws_subnet.id` is `string`.
        let ty = shape_with_schema(
            "value = [for s in aws_subnet.foo : s.id]",
            &FakeSchema,
        );
        assert_eq!(
            ty,
            VariableType::List(Box::new(VariableType::Primitive(Primitive::String)))
        );
    }

    #[test]
    fn schema_aware_for_expr_falls_back_to_any_for_unknown_resource() {
        // No schema entry for `unknown_thing` — element type stays
        // `any`, but we still produce `list(any)` rather than `any`.
        let ty = shape_with_schema(
            "value = [for s in unknown_thing.foo : s.id]",
            &FakeSchema,
        );
        assert_eq!(ty, VariableType::List(Box::new(VariableType::Any)));
    }

    #[test]
    fn schema_aware_map_for_expr_over_data_source() {
        let ty = shape_with_schema(
            "value = {for k, v in aws_subnet.foo : v.id => v.cidr_block}",
            &FakeSchema,
        );
        assert_eq!(
            ty,
            VariableType::Map(Box::new(VariableType::Primitive(Primitive::String)))
        );
    }
}
