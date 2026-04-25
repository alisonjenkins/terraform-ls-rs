//! Translate tfplugin5 protobuf types to the `tfls_schema` JSON shape.
//!
//! v5 is nearly identical to v6 except the `Attribute` message has no
//! `nested_type` field and a couple of newer `Schema.Block` / `Function`
//! fields are absent. The logic mirrors [`crate::translate`].

use std::collections::HashMap;

use tfls_schema::{AttributeSchema, BlockSchema, NestedBlockSchema, NestingMode, Schema};

use crate::ProtocolError;
use crate::proto_v5 as proto;
use crate::translate::cty_msgpack_to_json;

pub fn schema_from_proto(src: &proto::Schema) -> Result<Schema, ProtocolError> {
    let block = src
        .block
        .as_ref()
        .map(block_from_proto)
        .transpose()?
        .unwrap_or_default();
    Ok(Schema {
        version: u64::try_from(src.version).unwrap_or(0),
        block,
    })
}

fn block_from_proto(src: &proto::schema::Block) -> Result<BlockSchema, ProtocolError> {
    let mut attributes = HashMap::new();
    for attr in &src.attributes {
        attributes.insert(attr.name.clone(), attribute_from_proto(attr)?);
    }
    let mut block_types = HashMap::new();
    for nb in &src.block_types {
        if nb.block.is_none() {
            continue;
        }
        let nested = NestedBlockSchema {
            nesting_mode: nesting_mode_from_proto(nb.nesting()),
            block: block_from_proto(
                nb.block.as_ref().unwrap_or(&proto::schema::Block::default()),
            )?,
            min_items: u64::try_from(nb.min_items).unwrap_or(0),
            max_items: u64::try_from(nb.max_items).unwrap_or(0),
        };
        block_types.insert(nb.type_name.clone(), nested);
    }
    Ok(BlockSchema {
        attributes,
        block_types,
        description: non_empty(&src.description),
        description_kind: string_kind_name(src.description_kind()),
        deprecated: src.deprecated,
    })
}

fn attribute_from_proto(
    src: &proto::schema::Attribute,
) -> Result<AttributeSchema, ProtocolError> {
    let r#type = if !src.r#type.is_empty() {
        Some(cty_msgpack_to_json(&src.r#type)?)
    } else {
        None
    };
    Ok(AttributeSchema {
        r#type,
        description: non_empty(&src.description),
        description_kind: string_kind_name(src.description_kind()),
        required: src.required,
        optional: src.optional,
        computed: src.computed,
        sensitive: src.sensitive,
        deprecated: src.deprecated,
        conflicts_with: Vec::new(),
        required_with: Vec::new(),
        exactly_one_of: Vec::new(),
        at_least_one_of: Vec::new(),
    })
}

fn nesting_mode_from_proto(m: proto::schema::nested_block::NestingMode) -> NestingMode {
    use proto::schema::nested_block::NestingMode as P;
    match m {
        P::Single => NestingMode::Single,
        P::List => NestingMode::List,
        P::Set => NestingMode::Set,
        P::Map => NestingMode::Map,
        P::Group => NestingMode::Group,
        P::Invalid => NestingMode::Single,
    }
}

fn string_kind_name(k: proto::StringKind) -> Option<String> {
    match k {
        proto::StringKind::Plain => Some("plain".into()),
        proto::StringKind::Markdown => Some("markdown".into()),
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

pub fn function_from_proto(
    src: &proto::Function,
) -> Result<tfls_schema::FunctionSignature, ProtocolError> {
    let mut parameters = Vec::new();
    for p in &src.parameters {
        parameters.push(parameter_from_proto(p)?);
    }
    let variadic_parameter = src
        .variadic_parameter
        .as_ref()
        .map(parameter_from_proto)
        .transpose()?;
    let return_type = src
        .r#return
        .as_ref()
        .map(|r| cty_msgpack_to_json(&r.r#type))
        .transpose()?
        .unwrap_or(sonic_rs::Value::from("dynamic"));
    let description = if !src.description.is_empty() {
        Some(src.description.clone())
    } else if !src.summary.is_empty() {
        Some(src.summary.clone())
    } else {
        None
    };
    Ok(tfls_schema::FunctionSignature {
        description,
        return_type,
        parameters,
        variadic_parameter,
    })
}

fn parameter_from_proto(
    src: &proto::function::Parameter,
) -> Result<tfls_schema::FunctionParameter, ProtocolError> {
    Ok(tfls_schema::FunctionParameter {
        name: src.name.clone(),
        r#type: cty_msgpack_to_json(&src.r#type)?,
        description: non_empty(&src.description),
        is_nullable: src.allow_null_value,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// MessagePack for `"string"` — reused across attribute tests.
    const CTY_STRING: &[u8] = br#""string""#;

    #[test]
    fn schema_from_empty_proto() {
        let src = proto::Schema {
            version: 0,
            block: None,
        };
        let schema = schema_from_proto(&src).unwrap();
        assert_eq!(schema.version, 0);
        assert!(schema.block.attributes.is_empty());
    }

    #[test]
    fn schema_preserves_version() {
        let src = proto::Schema {
            version: 42,
            block: Some(proto::schema::Block::default()),
        };
        let schema = schema_from_proto(&src).unwrap();
        assert_eq!(schema.version, 42);
    }

    #[test]
    fn attribute_flags_round_trip() {
        let attr = proto::schema::Attribute {
            name: "test".into(),
            r#type: CTY_STRING.to_vec(),
            description: "a desc".into(),
            required: true,
            optional: false,
            computed: true,
            sensitive: true,
            deprecated: true,
            ..Default::default()
        };
        let result = attribute_from_proto(&attr).unwrap();
        assert!(result.required);
        assert!(!result.optional);
        assert!(result.computed);
        assert!(result.sensitive);
        assert!(result.deprecated);
        assert_eq!(result.description.as_deref(), Some("a desc"));
    }

    #[test]
    fn attribute_empty_description_becomes_none() {
        let attr = proto::schema::Attribute {
            name: "x".into(),
            r#type: CTY_STRING.to_vec(),
            description: String::new(),
            ..Default::default()
        };
        let result = attribute_from_proto(&attr).unwrap();
        assert!(result.description.is_none());
    }

    #[test]
    fn v5_attribute_has_empty_constraints() {
        let attr = proto::schema::Attribute {
            name: "x".into(),
            r#type: CTY_STRING.to_vec(),
            ..Default::default()
        };
        let result = attribute_from_proto(&attr).unwrap();
        assert!(result.conflicts_with.is_empty());
        assert!(result.required_with.is_empty());
        assert!(result.exactly_one_of.is_empty());
        assert!(result.at_least_one_of.is_empty());
    }

    #[test]
    fn nesting_mode_all_variants() {
        use proto::schema::nested_block::NestingMode as P;
        assert_eq!(nesting_mode_from_proto(P::Single), NestingMode::Single);
        assert_eq!(nesting_mode_from_proto(P::List), NestingMode::List);
        assert_eq!(nesting_mode_from_proto(P::Set), NestingMode::Set);
        assert_eq!(nesting_mode_from_proto(P::Map), NestingMode::Map);
        assert_eq!(nesting_mode_from_proto(P::Group), NestingMode::Group);
        assert_eq!(nesting_mode_from_proto(P::Invalid), NestingMode::Single);
    }

    #[test]
    fn string_kind_plain_and_markdown() {
        assert_eq!(
            string_kind_name(proto::StringKind::Plain).as_deref(),
            Some("plain")
        );
        assert_eq!(
            string_kind_name(proto::StringKind::Markdown).as_deref(),
            Some("markdown")
        );
    }

    #[test]
    fn function_with_params_and_return() {
        let func = proto::Function {
            parameters: vec![proto::function::Parameter {
                name: "arg1".into(),
                r#type: CTY_STRING.to_vec(),
                description: "first arg".into(),
                ..Default::default()
            }],
            variadic_parameter: Some(proto::function::Parameter {
                name: "rest".into(),
                r#type: CTY_STRING.to_vec(),
                ..Default::default()
            }),
            r#return: Some(proto::function::Return {
                r#type: CTY_STRING.to_vec(),
            }),
            summary: "a func".into(),
            ..Default::default()
        };
        let sig = function_from_proto(&func).unwrap();
        assert_eq!(sig.parameters.len(), 1);
        assert_eq!(sig.parameters[0].name, "arg1");
        assert!(sig.variadic_parameter.is_some());
        assert_eq!(sig.description.as_deref(), Some("a func"));
    }

    #[test]
    fn function_description_prefers_description_over_summary() {
        let func = proto::Function {
            description: "full desc".into(),
            summary: "short".into(),
            r#return: Some(proto::function::Return {
                r#type: CTY_STRING.to_vec(),
            }),
            ..Default::default()
        };
        let sig = function_from_proto(&func).unwrap();
        assert_eq!(sig.description.as_deref(), Some("full desc"));
    }

    #[test]
    fn function_defaults_return_type_to_dynamic() {
        let func = proto::Function::default();
        let sig = function_from_proto(&func).unwrap();
        use sonic_rs::JsonValueTrait;
        assert_eq!(sig.return_type.as_str(), Some("dynamic"));
    }
}
