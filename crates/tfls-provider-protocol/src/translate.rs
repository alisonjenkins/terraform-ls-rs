//! Translate tfplugin6 protobuf types into the `tfls_schema::*` JSON
//! shape that the rest of tfls already understands.
//!
//! The one subtlety is the `type` field on Attribute: the protocol sends
//! it as MessagePack-encoded `cty.Type`, whereas our existing
//! `tfls_schema::AttributeSchema.type` is a `sonic_rs::Value` shaped
//! like the `terraform providers schema -json` output (a string for
//! primitives or an array for compound types). So we decode the
//! msgpack bytes into `rmpv::Value`, then convert to a JSON value
//! matching the expected shape.

use std::collections::HashMap;

use tfls_schema::{AttributeSchema, BlockSchema, NestedBlockSchema, NestingMode, Schema};

use crate::ProtocolError;
use crate::proto;

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

pub fn block_from_proto(src: &proto::schema::Block) -> Result<BlockSchema, ProtocolError> {
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
            block: block_from_proto(nb.block.as_ref().unwrap_or(&proto::schema::Block::default()))?,
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
    // Describe either the scalar `type` or the `nested_type` (block-like
    // object), preferring the scalar when both are present.
    let r#type = if !src.r#type.is_empty() {
        Some(cty_msgpack_to_json(&src.r#type)?)
    } else if src.nested_type.is_some() {
        // Nested types are full object shapes; for hover/render purposes,
        // labelling them "object" is fine. The LSP hovers never actually
        // parse the type bytes themselves.
        Some(sonic_rs::Value::from("object"))
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
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// Decode a MessagePack-encoded `cty.Type` into the JSON shape that
/// matches `terraform providers schema -json`:
///
/// - A primitive becomes a string: `"string"`, `"number"`, `"bool"`,
///   `"dynamic"`.
/// - Compound types become arrays: `["list", "string"]`,
///   `["object", { "name": "string" }]`, etc.
///
/// We don't enforce full cty semantics — just pass the rmpv value
/// through as JSON, which preserves the shape.
pub fn cty_msgpack_to_json(bytes: &[u8]) -> Result<sonic_rs::Value, ProtocolError> {
    // Plugin protocol v5 and v6 both ship `Attribute.Type` as JSON
    // bytes (NOT msgpack), per the terraform-plugin-go contract:
    // > Type is the JSON-encoded type expression.
    // Earlier code used `rmpv` which silently mis-decoded the leading
    // `"` of `"string"` as a positive-fixint and turned every
    // primitive type into `Number(34)`. Parse as JSON directly.
    sonic_rs::from_slice(bytes)
        .map_err(|e| ProtocolError::CtyDecode(format!("parse json: {e}")))
}

/// Legacy msgpack decoder, retained for tests / fallback paths that
/// genuinely want msgpack semantics. Production schema decoding goes
/// through [`cty_msgpack_to_json`] which actually parses JSON bytes
/// per the plugin-protocol contract.
#[allow(dead_code)]
pub fn rmpv_msgpack_to_json(bytes: &[u8]) -> Result<sonic_rs::Value, ProtocolError> {
    let mut cursor = std::io::Cursor::new(bytes);
    let v = rmpv::decode::read_value(&mut cursor)
        .map_err(|e| ProtocolError::CtyDecode(e.to_string()))?;
    rmpv_to_sonic(v)
}

fn rmpv_to_sonic(v: rmpv::Value) -> Result<sonic_rs::Value, ProtocolError> {
    let s = rmpv_to_serde_json(v)?;
    // `sonic_rs::Value` parses JSON text; round-tripping via a string is
    // the simplest way to avoid maintaining a direct rmpv->sonic codec.
    let text = serde_json::to_string(&s)
        .map_err(|e| ProtocolError::CtyDecode(format!("serialize json: {e}")))?;
    sonic_rs::from_str(&text)
        .map_err(|e| ProtocolError::CtyDecode(format!("reparse json: {e}")))
}

fn rmpv_to_serde_json(v: rmpv::Value) -> Result<serde_json::Value, ProtocolError> {
    use rmpv::Value as R;
    Ok(match v {
        R::Nil => serde_json::Value::Null,
        R::Boolean(b) => serde_json::Value::Bool(b),
        R::Integer(i) => {
            if let Some(u) = i.as_u64() {
                serde_json::Value::Number(u.into())
            } else if let Some(s) = i.as_i64() {
                serde_json::Value::Number(s.into())
            } else if let Some(f) = i.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            }
        }
        R::F32(f) => serde_json::Number::from_f64(f as f64)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        R::F64(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        R::String(s) => {
            let text = s
                .into_str()
                .ok_or_else(|| ProtocolError::CtyDecode("non-utf8 string".into()))?;
            serde_json::Value::String(text)
        }
        R::Binary(b) => serde_json::Value::String(
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b),
        ),
        R::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(rmpv_to_serde_json(item)?);
            }
            serde_json::Value::Array(out)
        }
        R::Map(pairs) => {
            let mut out = serde_json::Map::new();
            for (k, v) in pairs {
                let key = match k {
                    R::String(s) => s
                        .into_str()
                        .ok_or_else(|| ProtocolError::CtyDecode("non-utf8 map key".into()))?,
                    other => other.to_string(),
                };
                out.insert(key, rmpv_to_serde_json(v)?);
            }
            serde_json::Value::Object(out)
        }
        R::Ext(_, _) => serde_json::Value::Null,
    })
}

/// Translate a tfplugin6 Function into our existing `FunctionSignature`
/// shape. Provider-defined functions are installed alongside built-ins,
/// keyed by a `provider::<ns>::<name>::<fn>` identifier by the caller.
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

    // Prefer the rich description; fall back to summary.
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

    /// Plugin protocol encodes `cty` types as JSON bytes — the
    /// primitive `string` arrives as the literal 8-byte JSON
    /// string `"string"`.
    #[test]
    fn decodes_string_primitive() {
        let bytes = br#""string""#;
        let v = cty_msgpack_to_json(bytes).unwrap();
        use sonic_rs::JsonValueTrait;
        assert_eq!(v.as_str(), Some("string"));
    }

    #[test]
    fn decodes_list_of_string() {
        let bytes = br#"["list","string"]"#;
        let v = cty_msgpack_to_json(bytes).unwrap();
        use sonic_rs::{JsonContainerTrait, JsonValueTrait};
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].as_str(), Some("list"));
        assert_eq!(arr[1].as_str(), Some("string"));
    }
}
