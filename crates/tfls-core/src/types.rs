use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// Unique identifier for a Terraform module — a directory containing `.tf` files.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleId(pub PathBuf);

impl ModuleId {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

/// A fully qualified provider address, e.g. `registry.terraform.io/hashicorp/aws`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderAddress {
    pub hostname: String,
    pub namespace: String,
    pub r#type: String,
}

impl ProviderAddress {
    pub fn new(
        hostname: impl Into<String>,
        namespace: impl Into<String>,
        r#type: impl Into<String>,
    ) -> Self {
        Self {
            hostname: hostname.into(),
            namespace: namespace.into(),
            r#type: r#type.into(),
        }
    }

    /// Default for HashiCorp-hosted providers.
    pub fn hashicorp(name: impl Into<String>) -> Self {
        Self::new("registry.terraform.io", "hashicorp", name)
    }

    /// Parse a provider address like `registry.terraform.io/hashicorp/aws`
    /// or the short form `hashicorp/aws` (assumes registry.terraform.io).
    pub fn parse(input: &str) -> Result<Self, CoreError> {
        let parts: Vec<&str> = input.split('/').collect();
        match parts.as_slice() {
            [hostname, namespace, type_] => Ok(Self::new(*hostname, *namespace, *type_)),
            [namespace, type_] => Ok(Self::new("registry.terraform.io", *namespace, *type_)),
            _ => Err(CoreError::InvalidProviderAddress {
                input: input.to_string(),
                reason: "expected 'host/namespace/type' or 'namespace/type'".to_string(),
            }),
        }
    }
}

impl std::fmt::Display for ProviderAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.hostname, self.namespace, self.r#type)
    }
}

/// A resource address like `aws_instance.web` or `data.aws_ami.ubuntu`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceAddress {
    pub resource_type: String,
    pub name: String,
}

impl ResourceAddress {
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            name: name.into(),
        }
    }
}

impl std::fmt::Display for ResourceAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.resource_type, self.name)
    }
}

/// The kind of a symbol in a Terraform module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Variable,
    Local,
    Output,
    Resource,
    DataSource,
    Module,
    Provider,
    TerraformBlock,
}

/// Location of a symbol, usable as a map key (Range doesn't implement Hash).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolLocation {
    pub uri: lsp_types::Url,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

impl SymbolLocation {
    pub fn new(uri: lsp_types::Url, range: lsp_types::Range) -> Self {
        Self {
            uri,
            start_line: range.start.line,
            start_character: range.start.character,
            end_line: range.end.line,
            end_character: range.end.character,
        }
    }

    pub fn range(&self) -> lsp_types::Range {
        lsp_types::Range {
            start: lsp_types::Position {
                line: self.start_line,
                character: self.start_character,
            },
            end: lsp_types::Position {
                line: self.end_line,
                character: self.end_character,
            },
        }
    }

    pub fn to_lsp_location(&self) -> lsp_types::Location {
        lsp_types::Location {
            uri: self.uri.clone(),
            range: self.range(),
        }
    }
}

/// A symbol in a Terraform module.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub location: SymbolLocation,
    pub detail: Option<String>,
    pub doc: Option<String>,
}

/// Per-module symbol table.
#[derive(Debug, Default, Clone)]
pub struct SymbolTable {
    pub variables: HashMap<String, Symbol>,
    pub locals: HashMap<String, Symbol>,
    pub outputs: HashMap<String, Symbol>,
    pub resources: HashMap<ResourceAddress, Symbol>,
    pub data_sources: HashMap<ResourceAddress, Symbol>,
    pub modules: HashMap<String, Symbol>,
    pub providers: HashMap<String, Symbol>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of symbols in the table.
    pub fn len(&self) -> usize {
        self.variables.len()
            + self.locals.len()
            + self.outputs.len()
            + self.resources.len()
            + self.data_sources.len()
            + self.modules.len()
            + self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn provider_address_parses_full_form() {
        let addr = ProviderAddress::parse("registry.terraform.io/hashicorp/aws")
            .expect("full form should parse");
        assert_eq!(addr.hostname, "registry.terraform.io");
        assert_eq!(addr.namespace, "hashicorp");
        assert_eq!(addr.r#type, "aws");
    }

    #[test]
    fn provider_address_parses_short_form() {
        let addr = ProviderAddress::parse("hashicorp/aws").expect("short form should parse");
        assert_eq!(addr.hostname, "registry.terraform.io");
        assert_eq!(addr.namespace, "hashicorp");
        assert_eq!(addr.r#type, "aws");
    }

    #[test]
    fn provider_address_rejects_invalid() {
        let err = ProviderAddress::parse("just-aws");
        assert!(matches!(err, Err(CoreError::InvalidProviderAddress { .. })));
    }

    #[test]
    fn provider_address_displays_canonically() {
        let addr = ProviderAddress::hashicorp("aws");
        assert_eq!(addr.to_string(), "registry.terraform.io/hashicorp/aws");
    }

    #[test]
    fn resource_address_displays() {
        let addr = ResourceAddress::new("aws_instance", "web");
        assert_eq!(addr.to_string(), "aws_instance.web");
    }

    #[test]
    fn symbol_table_is_empty_by_default() {
        let table = SymbolTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
    }
}
