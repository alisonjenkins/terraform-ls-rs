//! Parse Terraform `*.tfvars` / `*.tfvars.json` files.
//!
//! Tfvars files are flat `name = value` attribute lists — no blocks.
//! We don't index them as regular module documents (that would
//! confuse every diagnostic that walks blocks); instead we parse
//! once, infer the type of each RHS via `parse_value_shape`, and
//! return a `name → VariableType` map. The LSP layer then uses this
//! to back the "infer variable type" code action when a variable
//! has no `default` but is assigned in one or more tfvars files.
//!
//! Goes through [`crate::safe::parse_body`] like every other
//! hcl-edit parser entry point — see that module for the full
//! list of upstream panic sites we guard against. A single bad
//! tfvars file becomes an empty result + a structured `error!` log,
//! never an aborted tokio task.

use std::collections::HashMap;

use tfls_core::variable_type::{VariableType, parse_value_shape};

use crate::safe::parse_body;

/// Parse a tfvars HCL source body and return the inferred type for
/// each top-level assignment.
///
/// Heterogeneous-tuple / `Any` results are excluded from the returned
/// map: the caller (typically a code action) treats absence as
/// "don't know" and skips its suggestion. Object literals with
/// non-empty body are kept as `Object({…})`; empty `{}` and `[]`
/// are excluded for the same ambiguity reason as the
/// default-driven inference path.
pub fn parse_tfvars(source: &str) -> HashMap<String, VariableType> {
    let Ok(body) = parse_body(source) else {
        // Either a normal hcl-edit syntax error (returned `Err`) or a
        // panic ferried via `catch_unwind` (already logged in
        // `safe::catch`). Either way, we have nothing to infer from.
        return HashMap::new();
    };
    let mut out: HashMap<String, VariableType> = HashMap::new();
    for structure in body.iter() {
        let Some(attr) = structure.as_attribute() else {
            continue;
        };
        let name = attr.key.as_str().to_string();
        let ty = parse_value_shape(&attr.value);
        if matches!(&ty, VariableType::Any) {
            continue;
        }
        if let VariableType::Tuple(items) = &ty {
            if items.is_empty() {
                continue;
            }
        }
        if let VariableType::Object(fields) = &ty {
            if fields.is_empty() {
                continue;
            }
        }
        out.insert(name, ty);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_core::variable_type::Primitive;

    #[test]
    fn parses_primitive_assignments() {
        let m = parse_tfvars(
            "region = \"us-east-1\"\ncount = 3\nenabled = true\n",
        );
        assert_eq!(m.get("region"), Some(&VariableType::Primitive(Primitive::String)));
        assert_eq!(m.get("count"), Some(&VariableType::Primitive(Primitive::Number)));
        assert_eq!(m.get("enabled"), Some(&VariableType::Primitive(Primitive::Bool)));
    }

    #[test]
    fn parses_nested_object() {
        let m = parse_tfvars(
            "server = {\n  name = \"web\"\n  port = 8080\n}\n",
        );
        let ty = m.get("server").expect("server");
        match ty {
            VariableType::Object(fields) => {
                assert!(fields.contains_key("name"));
                assert!(fields.contains_key("port"));
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    #[test]
    fn skips_unresolvable_and_empty_collections() {
        // `var.x` reference, `[]`, `{}` all skip — too ambiguous.
        let m = parse_tfvars("a = var.x\nb = []\nc = {}\nd = \"keep\"\n");
        assert!(!m.contains_key("a"), "var.x should skip");
        assert!(!m.contains_key("b"), "[] should skip");
        assert!(!m.contains_key("c"), "{{}} should skip");
        assert!(m.contains_key("d"), "string should keep");
    }

    #[test]
    fn malformed_source_returns_empty() {
        let m = parse_tfvars("not valid {{");
        assert!(m.is_empty());
    }

    #[test]
    fn ignores_block_syntax() {
        // tfvars don't allow blocks, but if one sneaks in we skip it
        // (only `as_attribute` matches).
        let m = parse_tfvars("real = \"value\"\nblock {\n  ignored = 1\n}\n");
        assert_eq!(
            m.get("real"),
            Some(&VariableType::Primitive(Primitive::String))
        );
        assert!(!m.contains_key("ignored"));
    }
}
