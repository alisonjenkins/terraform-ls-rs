//! Schema-validation diagnostics for `resource` and `data` blocks.
//!
//! Given a parsed [`Body`] and a [`ProviderSchemas`] lookup, we emit:
//! - **Error**:   required attribute missing
//! - **Error**:   unknown attribute (not in schema)
//! - **Warning**: deprecated attribute in use

use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::{Diagnostic, DiagnosticSeverity, Url};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;
use tfls_schema::{ProviderSchemas, Schema};

/// How we look up a schema by (kind, type_name).
pub trait SchemaLookup {
    fn resource(&self, type_name: &str) -> Option<Schema>;
    fn data_source(&self, type_name: &str) -> Option<Schema>;
}

impl SchemaLookup for ProviderSchemas {
    fn resource(&self, type_name: &str) -> Option<Schema> {
        self.find_resource(type_name).map(|(_, s)| s.clone())
    }
    fn data_source(&self, type_name: &str) -> Option<Schema> {
        self.find_data_source(type_name).map(|(_, s)| s.clone())
    }
}

/// Walk the body and emit diagnostics for each `resource`/`data`
/// block that we have a schema for.
pub fn resource_diagnostics(
    body: &Body,
    rope: &Rope,
    _uri: &Url,
    lookup: &impl SchemaLookup,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    for structure in body.iter() {
        let block = match structure.as_block() {
            Some(b) => b,
            None => continue,
        };
        let ident = block.ident.as_str();

        let schema = match (ident, first_label(block)) {
            ("resource", Some(type_name)) => lookup.resource(type_name),
            ("data", Some(type_name)) => lookup.data_source(type_name),
            _ => None,
        };
        let Some(schema) = schema else { continue };

        validate_block(block, rope, &schema, &mut out);
    }

    out
}

fn validate_block(
    block: &Block,
    rope: &Rope,
    schema: &Schema,
    out: &mut Vec<Diagnostic>,
) {
    let Some(header_range) = header_range(block, rope) else {
        return;
    };

    // Attributes actually present in the body.
    let mut present_attrs: Vec<(&str, lsp_types::Range)> = Vec::new();
    for structure in block.body.iter() {
        if let Some(attr) = structure.as_attribute() {
            let name = attr.key.as_str();
            let span = attr.span().unwrap_or(0..0);
            let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
            present_attrs.push((name, range));
        }
    }

    // Deprecated / unknown checks.
    for (name, range) in &present_attrs {
        match schema.block.attributes.get(*name) {
            Some(attr) => {
                if attr.deprecated {
                    out.push(Diagnostic {
                        range: *range,
                        severity: Some(DiagnosticSeverity::WARNING),
                        source: Some("terraform-ls-rs".to_string()),
                        message: format!("attribute `{name}` is deprecated"),
                        ..Default::default()
                    });
                }
            }
            None => {
                // Allow nested blocks that happen to share a name.
                if schema.block.block_types.contains_key(*name) {
                    continue;
                }
                out.push(Diagnostic {
                    range: *range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!("unknown attribute `{name}`"),
                    ..Default::default()
                });
            }
        }
    }

    // Missing required.
    for (name, attr) in &schema.block.attributes {
        if attr.required
            && !present_attrs.iter().any(|(n, _)| *n == name.as_str())
        {
            out.push(Diagnostic {
                range: header_range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("terraform-ls-rs".to_string()),
                message: format!("missing required attribute `{name}`"),
                ..Default::default()
            });
        }
    }
}

fn first_label(block: &Block) -> Option<&str> {
    block.labels.first().map(|l| match l {
        BlockLabel::String(s) => s.value().as_str(),
        BlockLabel::Ident(i) => i.as_str(),
    })
}

fn header_range(block: &Block, rope: &Rope) -> Option<lsp_types::Range> {
    let span = block.ident.span()?;
    hcl_span_to_lsp_range(rope, span).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn uri() -> Url {
        Url::parse("file:///t.tf").expect("url")
    }

    fn schemas_aws_instance() -> ProviderSchemas {
        sonic_rs::from_str(
            r#"{
                "format_version": "1.0",
                "provider_schemas": {
                    "registry.terraform.io/hashicorp/aws": {
                        "provider": { "version": 0, "block": {} },
                        "resource_schemas": {
                            "aws_instance": {
                                "version": 1,
                                "block": {
                                    "attributes": {
                                        "ami":           { "type": "string", "required": true  },
                                        "instance_type": { "type": "string", "optional": true },
                                        "legacy_flag":   { "type": "bool",   "optional": true, "deprecated": true }
                                    }
                                }
                            }
                        }
                    }
                }
            }"#,
        )
        .expect("parse")
    }

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        resource_diagnostics(&body, &rope, &uri(), &schemas_aws_instance())
    }

    #[test]
    fn flags_missing_required() {
        let d = diags(r#"resource "aws_instance" "x" { instance_type = "t3.micro" }"#);
        assert!(
            d.iter().any(|d| d.message.contains("missing required") && d.message.contains("ami")),
            "got: {d:?}"
        );
    }

    #[test]
    fn flags_unknown_attribute() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami          = "ami-1"
          instance_type = "t3.micro"
          not_in_schema = true
        }"#);
        assert!(d.iter().any(|d| d.message.contains("unknown attribute `not_in_schema`")), "got: {d:?}");
    }

    #[test]
    fn flags_deprecated_attribute_as_warning() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami         = "ami-1"
          legacy_flag = true
        }"#);
        let dep = d
            .iter()
            .find(|d| d.message.contains("deprecated"))
            .expect("deprecation diagnostic");
        assert_eq!(dep.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn no_diagnostics_when_schema_missing() {
        let d = diags(r#"resource "unknown_type" "x" {}"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn valid_resource_yields_no_diagnostics() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami           = "ami-1"
          instance_type = "t3.micro"
        }"#);
        assert!(d.is_empty(), "got: {d:?}");
    }
}
