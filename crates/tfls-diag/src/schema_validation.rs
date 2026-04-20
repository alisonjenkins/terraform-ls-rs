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
use tfls_core::{BlockKind, CONDITION_ATTRS, is_meta_attr, lifecycle_attrs, lifecycle_blocks};
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
    uri: &Url,
    lookup: &impl SchemaLookup,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    for structure in body.iter() {
        let block = match structure.as_block() {
            Some(b) => b,
            None => continue,
        };
        let ident = block.ident.as_str();

        let (kind, schema) = match (ident, first_label(block)) {
            ("resource", Some(type_name)) => (BlockKind::Resource, lookup.resource(type_name)),
            ("data", Some(type_name)) => (BlockKind::Data, lookup.data_source(type_name)),
            _ => continue,
        };
        let Some(schema) = schema else { continue };

        validate_block(block, rope, uri, &schema, kind, &mut out);
    }

    out
}

/// True when the URI's path ends with `.tofu` or `.tofu.json` —
/// OpenTofu-only source files where OpenTofu-specific features can
/// be used without portability warnings.
fn is_opentofu_file(uri: &Url) -> bool {
    let path = uri.path();
    path.ends_with(".tofu") || path.ends_with(".tofu.json")
}

fn validate_block(
    block: &Block,
    rope: &Rope,
    uri: &Url,
    schema: &Schema,
    kind: BlockKind,
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
                // Terraform meta-arguments (count, for_each, provider,
                // depends_on) are valid in every resource/data block
                // even though providers don't declare them.
                if is_meta_attr(name) {
                    continue;
                }
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

    // Validate meta-blocks (lifecycle, provisioner, connection) that
    // are embedded directly in this resource/data body.
    for structure in block.body.iter() {
        let Some(inner) = structure.as_block() else {
            continue;
        };
        let name = inner.ident.as_str();
        match (kind, name) {
            (_, "lifecycle") => validate_lifecycle_block(inner, rope, uri, kind, out),
            (BlockKind::Resource, "provisioner") | (BlockKind::Resource, "connection") => {
                // Allowed; inner body is too variable to validate here.
            }
            _ => {
                // Provider-defined nested blocks or unknown blocks —
                // leave untouched for now.
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

/// Validate attributes and sub-blocks inside a `lifecycle { ... }` block.
/// The allowed names differ between resource and data blocks.
fn validate_lifecycle_block(
    block: &Block,
    rope: &Rope,
    uri: &Url,
    kind: BlockKind,
    out: &mut Vec<Diagnostic>,
) {
    let attrs = lifecycle_attrs(kind);
    let blocks = lifecycle_blocks(kind);
    let tofu_file = is_opentofu_file(uri);
    for structure in block.body.iter() {
        if let Some(attr) = structure.as_attribute() {
            let name = attr.key.as_str();
            if attrs.contains(&name) {
                // `enabled` is an OpenTofu-1.11+ meta-argument.
                // Accepted in `lifecycle_attrs` so the "unknown
                // attribute" pass stays quiet, but on a portable
                // `.tf` / `.tf.json` file its use is non-portable —
                // Terraform treats it as an unknown attribute at
                // plan time. Warn the author so they either rename
                // the file to `.tofu` or drop the feature.
                if name == "enabled" && !tofu_file {
                    let span = attr.span().unwrap_or(0..0);
                    let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
                    out.push(Diagnostic {
                        range,
                        severity: Some(DiagnosticSeverity::WARNING),
                        source: Some("terraform-ls-rs".to_string()),
                        message:
                            "`enabled` is an OpenTofu 1.11+ meta-argument; Terraform doesn't support it — rename this file to `.tofu` (or `.tofu.json`) if this module is OpenTofu-only, or use `count`/`for_each` for Terraform compatibility"
                                .to_string(),
                        ..Default::default()
                    });
                }
                continue;
            }
            let span = attr.span().unwrap_or(0..0);
            let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("terraform-ls-rs".to_string()),
                message: format!("unknown attribute `{name}`"),
                ..Default::default()
            });
        } else if let Some(inner) = structure.as_block() {
            let name = inner.ident.as_str();
            if blocks.contains(&name) {
                validate_condition_block(inner, rope, out);
            } else {
                let span = inner.ident.span().unwrap_or(0..0);
                let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
                out.push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!("unknown block `{name}`"),
                    ..Default::default()
                });
            }
        }
    }
}

/// Validate `precondition`/`postcondition` block bodies. Both accept
/// only `condition` and `error_message` attributes.
fn validate_condition_block(block: &Block, rope: &Rope, out: &mut Vec<Diagnostic>) {
    for structure in block.body.iter() {
        let Some(attr) = structure.as_attribute() else {
            continue;
        };
        let name = attr.key.as_str();
        if CONDITION_ATTRS.contains(&name) {
            continue;
        }
        let span = attr.span().unwrap_or(0..0);
        let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("terraform-ls-rs".to_string()),
            message: format!("unknown attribute `{name}`"),
            ..Default::default()
        });
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
                        },
                        "data_source_schemas": {
                            "aws_ami": {
                                "version": 0,
                                "block": {
                                    "attributes": {
                                        "id":    { "type": "string", "optional": true },
                                        "owners": { "type": ["list", "string"], "optional": true }
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

    // --- Meta-argument regression tests -------------------------------
    //
    // Terraform meta-arguments are language-level constructs valid in
    // every resource/data block regardless of provider schema. The
    // validator must not flag them as unknown attributes.

    fn has_unknown(d: &[Diagnostic], attr: &str) -> bool {
        let needle = format!("unknown attribute `{attr}`");
        d.iter().any(|diag| diag.message.contains(&needle))
    }

    #[test]
    fn meta_attr_count_not_flagged_in_resource() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami   = "ami-1"
          count = 2
        }"#);
        assert!(!has_unknown(&d, "count"), "got: {d:?}");
    }

    #[test]
    fn meta_attr_for_each_not_flagged_in_resource() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami      = "ami-1"
          for_each = toset(["a", "b"])
        }"#);
        assert!(!has_unknown(&d, "for_each"), "got: {d:?}");
    }

    #[test]
    fn meta_attr_provider_not_flagged_in_resource() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami      = "ami-1"
          provider = aws.east
        }"#);
        assert!(!has_unknown(&d, "provider"), "got: {d:?}");
    }

    #[test]
    fn meta_attr_depends_on_not_flagged_in_resource() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami        = "ami-1"
          depends_on = []
        }"#);
        assert!(!has_unknown(&d, "depends_on"), "got: {d:?}");
    }

    #[test]
    fn meta_attrs_not_flagged_in_data_block() {
        let d = diags(r#"data "aws_ami" "x" {
          count      = 1
          for_each   = toset(["a"])
          provider   = aws.east
          depends_on = []
        }"#);
        assert!(!has_unknown(&d, "count"), "got: {d:?}");
        assert!(!has_unknown(&d, "for_each"), "got: {d:?}");
        assert!(!has_unknown(&d, "provider"), "got: {d:?}");
        assert!(!has_unknown(&d, "depends_on"), "got: {d:?}");
    }

    #[test]
    fn truly_unknown_attribute_is_still_flagged() {
        // Negative regression: the meta-argument fix must not over-match.
        let d = diags(r#"resource "aws_instance" "x" {
          ami           = "ami-1"
          not_in_schema = true
        }"#);
        assert!(has_unknown(&d, "not_in_schema"), "got: {d:?}");
    }

    #[test]
    fn lifecycle_block_with_known_attrs_is_accepted_in_resource() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami = "ami-1"
          lifecycle {
            create_before_destroy = true
            prevent_destroy       = false
          }
        }"#);
        assert!(
            d.iter().all(|diag| !diag.message.contains("unknown")),
            "got: {d:?}"
        );
    }

    #[test]
    fn lifecycle_unknown_attr_is_flagged_in_resource() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami = "ami-1"
          lifecycle {
            typo = true
          }
        }"#);
        assert!(has_unknown(&d, "typo"), "got: {d:?}");
    }

    // `enabled` is OpenTofu 1.11+ only. In `.tf` / `.tf.json` files
    // it should warn (not error — rename to `.tofu` is a valid fix,
    // so the file isn't strictly broken). In `.tofu` / `.tofu.json`
    // files it's silent.

    fn diags_with_uri(src: &str, u: &Url) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        resource_diagnostics(&body, &rope, u, &schemas_aws_instance())
    }

    #[test]
    fn lifecycle_enabled_warns_in_tf_file() {
        let u = Url::parse("file:///m/main.tf").expect("url");
        let d = diags_with_uri(
            r#"resource "aws_instance" "x" {
              ami = "ami-1"
              lifecycle {
                enabled = true
              }
            }"#,
            &u,
        );
        let diag = d
            .iter()
            .find(|x| x.message.contains("OpenTofu"))
            .expect("expected an OpenTofu warning");
        assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
        // Not flagged as unknown — just warned about portability.
        assert!(!has_unknown(&d, "enabled"), "should not be 'unknown': {d:?}");
    }

    #[test]
    fn lifecycle_enabled_warns_in_tf_json_file() {
        let u = Url::parse("file:///m/main.tf.json").expect("url");
        let d = diags_with_uri(
            r#"resource "aws_instance" "x" {
              ami = "ami-1"
              lifecycle {
                enabled = true
              }
            }"#,
            &u,
        );
        assert!(
            d.iter().any(|x| x.message.contains("OpenTofu")),
            "got: {d:?}"
        );
    }

    #[test]
    fn lifecycle_enabled_silent_in_tofu_file() {
        let u = Url::parse("file:///m/main.tofu").expect("url");
        let d = diags_with_uri(
            r#"resource "aws_instance" "x" {
              ami = "ami-1"
              lifecycle {
                enabled = true
              }
            }"#,
            &u,
        );
        assert!(
            d.iter().all(|x| !x.message.contains("OpenTofu")),
            "got: {d:?}"
        );
        assert!(!has_unknown(&d, "enabled"), "got: {d:?}");
    }

    #[test]
    fn lifecycle_enabled_silent_in_tofu_json_file() {
        let u = Url::parse("file:///m/main.tofu.json").expect("url");
        let d = diags_with_uri(
            r#"resource "aws_instance" "x" {
              ami = "ami-1"
              lifecycle {
                enabled = true
              }
            }"#,
            &u,
        );
        assert!(
            d.iter().all(|x| !x.message.contains("OpenTofu")),
            "got: {d:?}"
        );
    }

    #[test]
    fn lifecycle_enabled_in_data_warns_in_tf_file() {
        let u = Url::parse("file:///m/main.tf").expect("url");
        let d = diags_with_uri(
            r#"data "aws_ami" "x" {
              lifecycle {
                enabled = true
              }
            }"#,
            &u,
        );
        assert!(
            d.iter().any(|x| x.message.contains("OpenTofu")),
            "got: {d:?}"
        );
    }

    #[test]
    fn lifecycle_data_postcondition_is_accepted() {
        let d = diags(r#"data "aws_ami" "x" {
          lifecycle {
            postcondition {
              condition     = true
              error_message = "nope"
            }
          }
        }"#);
        assert!(
            d.iter().all(|diag| !diag.message.contains("unknown")),
            "got: {d:?}"
        );
    }

    #[test]
    fn lifecycle_data_attrs_not_allowed() {
        // `create_before_destroy` only valid on resources, not data sources.
        let d = diags(r#"data "aws_ami" "x" {
          lifecycle {
            create_before_destroy = true
          }
        }"#);
        assert!(has_unknown(&d, "create_before_destroy"), "got: {d:?}");
    }

    #[test]
    fn provisioner_block_body_not_validated() {
        // provisioner bodies vary per provisioner type; skip inner checks.
        let d = diags(r#"resource "aws_instance" "x" {
          ami = "ami-1"
          provisioner "local-exec" {
            command = "echo hi"
            anything_goes = true
          }
        }"#);
        assert!(
            d.iter().all(|diag| !diag.message.contains("unknown")),
            "got: {d:?}"
        );
    }

    #[test]
    fn connection_block_body_not_validated() {
        let d = diags(r#"resource "aws_instance" "x" {
          ami = "ami-1"
          connection {
            type = "ssh"
            host = "h"
          }
        }"#);
        assert!(
            d.iter().all(|diag| !diag.message.contains("unknown")),
            "got: {d:?}"
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
