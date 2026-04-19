//! Structural types parsed from Terraform `variable "…" { type = … }`
//! declarations. Used to power attribute completion on `var.NAME.field…`.

use std::collections::BTreeMap;

use hcl_edit::expr::Expression;

/// A Terraform variable type — the shape declared via `type = …` in a
/// `variable` block. Only `Object` carries drill-in information for
/// completion, but the other variants exist so we can faithfully
/// represent any legal type expression (useful for future hover/diag
/// features).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableType {
    Any,
    Primitive(Primitive),
    List(Box<VariableType>),
    Set(Box<VariableType>),
    Map(Box<VariableType>),
    Tuple(Vec<VariableType>),
    Object(BTreeMap<String, VariableType>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub fn parse_value_shape(expr: &Expression) -> VariableType {
    match expr {
        Expression::Object(obj) => {
            let mut fields = BTreeMap::new();
            for (key, value) in obj.iter() {
                let Some(name) = object_key_as_ident(key) else {
                    continue;
                };
                fields.insert(name, parse_value_shape(value.expr()));
            }
            VariableType::Object(fields)
        }
        Expression::Array(arr) => {
            let items: Vec<VariableType> = arr.iter().map(parse_value_shape).collect();
            VariableType::Tuple(items)
        }
        Expression::String(_) => VariableType::Primitive(Primitive::String),
        Expression::Number(_) => VariableType::Primitive(Primitive::Number),
        Expression::Bool(_) => VariableType::Primitive(Primitive::Bool),
        Expression::FuncCall(call) => {
            if !call.name.namespace.is_empty() {
                return VariableType::Any;
            }
            match call.name.name.as_str() {
                "toset" | "tolist" => {
                    if let Some(first) = call.args.iter().next() {
                        if let Expression::Array(arr) = first {
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
                    }
                    VariableType::Any
                }
                "tomap" => call
                    .args
                    .iter()
                    .next()
                    .map(parse_value_shape)
                    .unwrap_or(VariableType::Any),
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
                            fields.insert(name, parse_value_shape(value.expr()));
                        }
                        return VariableType::Object(fields);
                    }
                    VariableType::Any
                }
                _ => VariableType::Any,
            }
        }
        Expression::Conditional(c) => merge_shapes(
            parse_value_shape(&c.true_expr),
            parse_value_shape(&c.false_expr),
        ),
        Expression::Parenthesis(inner) => parse_value_shape(inner.inner()),
        _ => VariableType::Any,
    }
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
}
