//! Types mirroring the `terraform providers schema -json` output.
//!
//! Deserialised with `sonic_rs`, which uses SIMD acceleration and is
//! significantly faster than `serde_json` for the large schema
//! documents (10-50MB) emitted by providers like `hashicorp/aws`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Top-level document produced by `terraform providers schema -json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSchemas {
    pub format_version: String,
    #[serde(default)]
    pub provider_schemas: HashMap<String, ProviderSchema>,
}

/// Schema for a single provider: its own provider block, resources,
/// data sources, and functions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSchema {
    pub provider: Schema,
    #[serde(default)]
    pub resource_schemas: HashMap<String, Schema>,
    #[serde(default)]
    pub data_source_schemas: HashMap<String, Schema>,
}

/// A versioned schema for a block type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    #[serde(default)]
    pub version: u64,
    pub block: BlockSchema,
}

/// A block's attributes and nested block types.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockSchema {
    #[serde(default)]
    pub attributes: HashMap<String, AttributeSchema>,
    #[serde(default)]
    pub block_types: HashMap<String, NestedBlockSchema>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub description_kind: Option<String>,
    #[serde(default)]
    pub deprecated: bool,
}

/// Attribute schema (required/optional/computed + type).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttributeSchema {
    /// `type` is reserved in Rust so we rename it.
    #[serde(rename = "type", default)]
    pub r#type: Option<SchemaType>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub description_kind: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub computed: bool,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub deprecated: bool,

    // Relational constraints. The CLI JSON emits these for some providers;
    // the plugin gRPC protocol returns them on every attribute. Defaults
    // make old schemas still deserialise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts_with: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_with: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exactly_one_of: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub at_least_one_of: Vec<String>,
}

/// How a nested block relates to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NestingMode {
    Single,
    List,
    Set,
    Map,
    Group,
}

/// A block type nested inside another block (e.g. `lifecycle` inside
/// a `resource` block).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NestedBlockSchema {
    pub nesting_mode: NestingMode,
    pub block: BlockSchema,
    #[serde(default)]
    pub min_items: u64,
    #[serde(default)]
    pub max_items: u64,
}

/// Terraform's cty types come in as either a string (primitive) or a
/// JSON array (compound). We store the raw JSON value — callers can
/// inspect it as needed.
pub type SchemaType = sonic_rs::Value;

impl ProviderSchemas {
    /// Look up a resource schema by unqualified type name (e.g. `aws_instance`).
    ///
    /// Terraform schema keys are qualified provider addresses; we
    /// search across all providers for the first matching entry.
    pub fn find_resource(&self, type_name: &str) -> Option<(&str, &Schema)> {
        for (provider, schema) in &self.provider_schemas {
            if let Some(s) = schema.resource_schemas.get(type_name) {
                return Some((provider.as_str(), s));
            }
        }
        None
    }

    /// Look up a data source schema by unqualified type name.
    pub fn find_data_source(&self, type_name: &str) -> Option<(&str, &Schema)> {
        for (provider, schema) in &self.provider_schemas {
            if let Some(s) = schema.data_source_schemas.get(type_name) {
                return Some((provider.as_str(), s));
            }
        }
        None
    }

    /// Iterate all known resource type names across all providers.
    pub fn all_resource_types(&self) -> impl Iterator<Item = &str> {
        self.provider_schemas
            .values()
            .flat_map(|p| p.resource_schemas.keys().map(String::as_str))
    }

    /// Iterate all known data source type names across all providers.
    pub fn all_data_source_types(&self) -> impl Iterator<Item = &str> {
        self.provider_schemas
            .values()
            .flat_map(|p| p.data_source_schemas.keys().map(String::as_str))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const MINIMAL_SCHEMA: &str = r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/hashicorp/aws": {
                "provider": {
                    "version": 0,
                    "block": { "attributes": {}, "block_types": {} }
                },
                "resource_schemas": {
                    "aws_instance": {
                        "version": 1,
                        "block": {
                            "attributes": {
                                "ami": {
                                    "type": "string",
                                    "description": "The AMI ID",
                                    "required": true
                                },
                                "tags": {
                                    "type": ["map", "string"],
                                    "optional": true
                                }
                            },
                            "block_types": {
                                "lifecycle": {
                                    "nesting_mode": "single",
                                    "block": { "attributes": {}, "block_types": {} }
                                }
                            }
                        }
                    }
                },
                "data_source_schemas": {
                    "aws_ami": {
                        "version": 0,
                        "block": { "attributes": {}, "block_types": {} }
                    }
                }
            }
        }
    }"#;

    #[test]
    fn deserialises_minimal_schema() {
        let schemas: ProviderSchemas = sonic_rs::from_str(MINIMAL_SCHEMA).expect("parse");
        assert_eq!(schemas.format_version, "1.0");
        assert_eq!(schemas.provider_schemas.len(), 1);
    }

    #[test]
    fn finds_resource_across_providers() {
        let schemas: ProviderSchemas = sonic_rs::from_str(MINIMAL_SCHEMA).expect("parse");
        let (provider, resource) = schemas
            .find_resource("aws_instance")
            .expect("aws_instance should be found");
        assert!(provider.contains("hashicorp/aws"));
        assert!(resource.block.attributes.contains_key("ami"));
        assert!(resource.block.attributes["ami"].required);
    }

    #[test]
    fn finds_data_source() {
        let schemas: ProviderSchemas = sonic_rs::from_str(MINIMAL_SCHEMA).expect("parse");
        assert!(schemas.find_data_source("aws_ami").is_some());
        assert!(schemas.find_data_source("aws_instance").is_none());
    }

    #[test]
    fn iterates_all_types() {
        let schemas: ProviderSchemas = sonic_rs::from_str(MINIMAL_SCHEMA).expect("parse");
        let resources: Vec<_> = schemas.all_resource_types().collect();
        assert_eq!(resources, vec!["aws_instance"]);
        let data: Vec<_> = schemas.all_data_source_types().collect();
        assert_eq!(data, vec!["aws_ami"]);
    }

    #[test]
    fn nested_block_types_parse() {
        let schemas: ProviderSchemas = sonic_rs::from_str(MINIMAL_SCHEMA).expect("parse");
        let (_, res) = schemas.find_resource("aws_instance").unwrap();
        let lifecycle = res
            .block
            .block_types
            .get("lifecycle")
            .expect("lifecycle present");
        assert_eq!(lifecycle.nesting_mode, NestingMode::Single);
    }

    #[test]
    fn attribute_schema_defaults_are_false() {
        let json = r#"{ "type": "string" }"#;
        let attr: AttributeSchema = sonic_rs::from_str(json).expect("parse");
        assert!(!attr.required);
        assert!(!attr.optional);
        assert!(!attr.computed);
        assert!(!attr.sensitive);
        assert!(!attr.deprecated);
    }
}
