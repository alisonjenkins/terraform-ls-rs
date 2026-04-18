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

fn object_key_as_ident(key: &hcl_edit::expr::ObjectKey) -> Option<String> {
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

    #[test]
    fn display_renders_concise_form() {
        let ty = type_from_src("type = object({ name = string, inner = object({ x = bool }) })");
        let rendered = format!("{ty}");
        // BTreeMap sorts keys alphabetically: "inner" < "name".
        assert_eq!(rendered, "object({ inner = object({ x = bool }), name = string })");
    }
}
