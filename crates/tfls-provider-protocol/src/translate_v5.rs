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
