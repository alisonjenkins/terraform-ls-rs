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

    #[test]
    fn display_renders_concise_form() {
        let ty = type_from_src("type = object({ name = string, inner = object({ x = bool }) })");
        let rendered = format!("{ty}");
        // BTreeMap sorts keys alphabetically: "inner" < "name".
        assert_eq!(rendered, "object({ inner = object({ x = bool }), name = string })");
    }
}
