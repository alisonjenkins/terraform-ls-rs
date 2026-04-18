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

    // Relational constraints from the schema (CLI JSON emits these for some
    // providers; plugin-protocol doesn't yet, so many blocks will have empty
    // lists and this is a no-op).
    for (name, range) in &present_attrs {
        let Some(attr) = schema.block.attributes.get(*name) else {
            continue;
        };

        for other in &attr.conflicts_with {
            if present_attrs.iter().any(|(n, _)| *n == other.as_str()) {
                out.push(Diagnostic {
                    range: *range,
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!(
                        "attribute `{name}` conflicts with `{other}` — set one, not both"
                    ),
                    ..Default::default()
                });
            }
        }

        for other in &attr.required_with {
            if !present_attrs.iter().any(|(n, _)| *n == other.as_str()) {
                out.push(Diagnostic {
                    range: *range,
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!(
                        "attribute `{name}` requires `{other}` to also be set"
                    ),
                    ..Default::default()
                });
            }
        }

        for other in &attr.exactly_one_of {
            if other == *name {
                continue;
            }
            if present_attrs.iter().any(|(n, _)| *n == other.as_str()) {
                out.push(Diagnostic {
                    range: *range,
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!(
                        "attribute `{name}` and `{other}` are in the same exactly-one-of group — set exactly one"
                    ),
                    ..Default::default()
                });
            }
        }
    }

    // at_least_one_of: if no member of the group is present, warn once
    // per unique group. Dedupe by sorting the group members.
    let mut seen_groups: Vec<Vec<String>> = Vec::new();
    for (attr_name, attr) in &schema.block.attributes {
        if attr.at_least_one_of.is_empty() {
            continue;
        }
        let mut group: Vec<String> = attr.at_least_one_of.clone();
        if !group.contains(attr_name) {
            group.push(attr_name.clone());
        }
        group.sort();
        if seen_groups.contains(&group) {
            continue;
        }
        let any_present = group
            .iter()
            .any(|member| present_attrs.iter().any(|(n, _)| *n == member.as_str()));
        if !any_present {
            let members = group
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Diagnostic {
                range: header_range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message: format!("at least one of {members} must be set"),
                ..Default::default()
            });
        }
        seen_groups.push(group);
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

    fn schemas_with_relations() -> ProviderSchemas {
        sonic_rs::from_str(
            r#"{
                "format_version": "1.0",
                "provider_schemas": {
                    "registry.terraform.io/hashicorp/aws": {
                        "provider": { "version": 0, "block": {} },
                        "resource_schemas": {
                            "aws_thing": {
                                "version": 1,
                                "block": {
                                    "attributes": {
                                        "a": { "type": "string", "optional": true, "conflicts_with": ["b"] },
                                        "b": { "type": "string", "optional": true, "conflicts_with": ["a"] },
                                        "c": { "type": "string", "optional": true, "required_with": ["d"] },
                                        "d": { "type": "string", "optional": true },
                                        "e": { "type": "string", "optional": true, "exactly_one_of": ["e", "f"] },
                                        "f": { "type": "string", "optional": true, "exactly_one_of": ["e", "f"] },
                                        "g": { "type": "string", "optional": true, "at_least_one_of": ["g", "h"] },
                                        "h": { "type": "string", "optional": true, "at_least_one_of": ["g", "h"] }
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

    fn diags_with(schemas: &ProviderSchemas, src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        resource_diagnostics(&body, &rope, &uri(), schemas)
    }

    #[test]
    fn flags_conflicts_with() {
        let schemas = schemas_with_relations();
        let d = diags_with(
            &schemas,
            r#"resource "aws_thing" "x" {
              a = "one"
              b = "two"
            }"#,
        );
        let conflict = d
            .iter()
            .find(|d| d.message.contains("conflicts with"))
            .expect("conflict diagnostic");
        assert_eq!(conflict.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn flags_missing_required_with() {
        let schemas = schemas_with_relations();
        let d = diags_with(
            &schemas,
            r#"resource "aws_thing" "x" {
              c = "one"
            }"#,
        );
        let req = d
            .iter()
            .find(|d| d.message.contains("requires `d`"))
            .expect("required-with diagnostic");
        assert_eq!(req.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn flags_exactly_one_of_when_both_set() {
        let schemas = schemas_with_relations();
        let d = diags_with(
            &schemas,
            r#"resource "aws_thing" "x" {
              e = "one"
              f = "two"
            }"#,
        );
        let exactly = d
            .iter()
            .find(|d| d.message.contains("exactly-one-of"))
            .expect("exactly-one-of diagnostic");
        assert_eq!(exactly.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn exactly_one_of_with_one_set_is_ok() {
        let schemas = schemas_with_relations();
        let d = diags_with(
            &schemas,
            r#"resource "aws_thing" "x" {
              e = "one"
            }"#,
        );
        assert!(
            d.iter().all(|d| !d.message.contains("exactly-one-of")),
            "unexpected exactly-one-of warning: {d:?}"
        );
    }

    #[test]
    fn flags_at_least_one_of_when_none_set() {
        let schemas = schemas_with_relations();
        let d = diags_with(
            &schemas,
            r#"resource "aws_thing" "x" {
              a = "one"
            }"#,
        );
        let at_least = d
            .iter()
            .find(|d| d.message.contains("at least one of"))
            .expect("at-least-one-of diagnostic");
        assert_eq!(at_least.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn at_least_one_of_satisfied() {
        let schemas = schemas_with_relations();
        let d = diags_with(
            &schemas,
            r#"resource "aws_thing" "x" {
              g = "one"
            }"#,
        );
        assert!(
            d.iter().all(|d| !d.message.contains("at least one of")),
            "unexpected at-least-one-of warning: {d:?}"
        );
    }

    #[test]
    fn required_with_satisfied_yields_no_diagnostic() {
        let schemas = schemas_with_relations();
        let d = diags_with(
            &schemas,
            r#"resource "aws_thing" "x" {
              c = "one"
              d = "two"
            }"#,
        );
        assert!(
            d.iter().all(|d| !d.message.contains("requires")),
            "unexpected required-with warning: {d:?}"
        );
    }
}
