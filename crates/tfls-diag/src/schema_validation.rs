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
use tfls_schema::{BlockSchema, ProviderSchemas, Schema};

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

        let (kind, schema) = match (ident, first_label(block)) {
            ("resource", Some(type_name)) => (BlockKind::Resource, lookup.resource(type_name)),
            ("data", Some(type_name)) => (BlockKind::Data, lookup.data_source(type_name)),
            _ => continue,
        };
        let Some(schema) = schema else { continue };

        validate_block(block, rope, &schema, kind, &mut out);
    }

    out
}

fn validate_block(
    block: &Block,
    rope: &Rope,
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

    // Validate meta-blocks (lifecycle, provisioner, connection,
    // dynamic) that are embedded directly in this resource/data body.
    for structure in block.body.iter() {
        let Some(inner) = structure.as_block() else {
            continue;
        };
        let name = inner.ident.as_str();
        match (kind, name) {
            (_, "lifecycle") => validate_lifecycle_block(inner, rope, kind, out),
            (_, "dynamic") => validate_dynamic_block(inner, &schema.block, rope, out),
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
    kind: BlockKind,
    out: &mut Vec<Diagnostic>,
) {
    let attrs = lifecycle_attrs(kind);
    let blocks = lifecycle_blocks(kind);
    for structure in block.body.iter() {
        if let Some(attr) = structure.as_attribute() {
            let name = attr.key.as_str();
            if attrs.contains(&name) {
                // `enabled` is an OpenTofu-1.11+ meta-argument;
                // listed in `lifecycle_attrs` so we don't flag it
                // as "unknown attribute". Portability feedback
                // (OpenTofu vs Terraform) is surfaced as an inlay
                // hint in `handlers/inlay_hints.rs`, not a
                // diagnostic — see that file for the emitter.
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

/// Validate a `dynamic "<label>" { for_each = …; content { … } }`
/// meta-block embedded in a resource / data body. Emits:
///
/// - `unknown nested block …` (error, on the label span) when the
///   label isn't a real nested block type in the parent schema.
/// - `missing required \`for_each\`` (error, on the `dynamic` ident)
///   when the for_each attr is absent from the dynamic body.
/// - `missing required attribute \`N\` inside \`content\`` (error,
///   on the `content` ident) once per required attr the target
///   nested block declares but `content {}` doesn't set.
fn validate_dynamic_block(
    block: &Block,
    parent_schema: &BlockSchema,
    rope: &Rope,
    out: &mut Vec<Diagnostic>,
) {
    let Some(label) = first_label(block) else {
        // Parse-level error (no label) — surface is elsewhere.
        return;
    };
    let label_owned = label.to_string();

    // 1. Resolve the target nested block type in the parent schema.
    //    Unknown → error on the label span; bail, since we can't
    //    validate required attrs against a schema we don't have.
    let Some(nb) = parent_schema.block_types.get(&label_owned) else {
        if let Some(first) = block.labels.first() {
            if let Some(span) = first.span() {
                let range = hcl_span_to_lsp_range(rope, span).unwrap_or_default();
                out.push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!(
                        "unknown nested block `{label_owned}` on this resource/data"
                    ),
                    ..Default::default()
                });
            }
        }
        return;
    };
    let target = &nb.block;

    // 2. Scan the dynamic body: record for_each presence and find
    //    the content { } child (if any).
    let mut has_for_each = false;
    let mut content_block: Option<&Block> = None;
    for structure in block.body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if attr.key.as_str() == "for_each" {
                has_for_each = true;
            }
        } else if let Some(inner) = structure.as_block() {
            if inner.ident.as_str() == "content" {
                content_block = Some(inner);
            }
        }
    }

    // 3. for_each is mandatory.
    if !has_for_each {
        if let Some(range) = header_range(block, rope) {
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("terraform-ls-rs".to_string()),
                message: format!(
                    "dynamic \"{label_owned}\" missing required `for_each`"
                ),
                ..Default::default()
            });
        }
    }

    // 4. Required attrs of the target block must appear inside
    //    `content { }`. No content block? The language requires it
    //    but that's a parser-level concern — we still report missing
    //    required attrs with the dynamic header as the anchor so the
    //    user sees which attrs are outstanding.
    let content_ident_range = match &content_block {
        Some(c) => c
            .ident
            .span()
            .and_then(|s| hcl_span_to_lsp_range(rope, s).ok()),
        None => None,
    };
    let mut content_attrs: Vec<&str> = Vec::new();
    if let Some(content) = content_block {
        for structure in content.body.iter() {
            if let Some(attr) = structure.as_attribute() {
                content_attrs.push(attr.key.as_str());
            }
        }
    }

    for (attr_name, attr_schema) in &target.attributes {
        if !attr_schema.required {
            continue;
        }
        if content_attrs.contains(&attr_name.as_str()) {
            continue;
        }
        let range = content_ident_range
            .or_else(|| header_range(block, rope))
            .unwrap_or_default();
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("terraform-ls-rs".to_string()),
            message: format!(
                "dynamic \"{label_owned}\" — missing required attribute `{attr_name}` inside `content`"
            ),
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

    // `enabled` is OpenTofu 1.11+ only. It's accepted in
    // `lifecycle_attrs` so this pass doesn't flag it as "unknown
    // attribute"; portability feedback lives in the inlay-hints
    // module, not here. Two minimal regressions: presence doesn't
    // produce a diagnostic in either file kind.

    #[test]
    fn lifecycle_enabled_is_not_flagged_as_unknown() {
        let d = diags(
            r#"resource "aws_instance" "x" {
              ami = "ami-1"
              lifecycle {
                enabled = true
              }
            }"#,
        );
        assert!(!has_unknown(&d, "enabled"), "got: {d:?}");
        assert!(
            d.iter().all(|x| !x.message.contains("OpenTofu")),
            "portability feedback should be an inlay hint, not a diagnostic: {d:?}"
        );
    }

    #[test]
    fn lifecycle_enabled_in_data_is_not_flagged() {
        let d = diags(
            r#"data "aws_ami" "x" {
              lifecycle {
                enabled = true
              }
            }"#,
        );
        assert!(!has_unknown(&d, "enabled"), "got: {d:?}");
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

    // --- Dynamic-block regression tests ------------------------------
    //
    // `dynamic "<label>" { for_each = …; content { … } }` is a
    // language meta-construct. We validate it against the target
    // nested block's schema in the enclosing resource / data.

    fn schemas_with_ebs_block_device() -> ProviderSchemas {
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
                                        "ami": { "type": "string", "required": true }
                                    },
                                    "block_types": {
                                        "ebs_block_device": {
                                            "nesting_mode": "list",
                                            "block": {
                                                "attributes": {
                                                    "device_name": { "type": "string", "required": true },
                                                    "volume_size": { "type": "number", "optional": true }
                                                }
                                            }
                                        }
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

    #[test]
    fn dynamic_missing_for_each_is_flagged() {
        let schemas = schemas_with_ebs_block_device();
        let src = r#"resource "aws_instance" "x" {
              ami = "ami-1"
              dynamic "ebs_block_device" {
                content {
                  device_name = "/dev/sda1"
                }
              }
            }"#;
        let d = diags_with(&schemas, src);
        let hit = d
            .iter()
            .find(|diag| diag.message.contains("missing required `for_each`"))
            .expect("for_each missing diagnostic");
        assert_eq!(hit.severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn dynamic_missing_required_attr_inside_content_is_flagged() {
        let schemas = schemas_with_ebs_block_device();
        // `device_name` is required on ebs_block_device — the dynamic
        // content body must set it. Omit it and expect a diagnostic
        // naming both the dynamic label and the attribute.
        let src = r#"resource "aws_instance" "x" {
              ami = "ami-1"
              dynamic "ebs_block_device" {
                for_each = []
                content {
                  volume_size = 20
                }
              }
            }"#;
        let d = diags_with(&schemas, src);
        let hit = d
            .iter()
            .find(|diag| {
                diag.message.contains("device_name")
                    && diag.message.contains("ebs_block_device")
                    && diag.message.contains("content")
            })
            .expect("required-attr-in-content diagnostic");
        assert_eq!(hit.severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn dynamic_unknown_label_is_flagged() {
        let schemas = schemas_with_ebs_block_device();
        // `nonsense_block` isn't a nested block on aws_instance.
        let src = r#"resource "aws_instance" "x" {
              ami = "ami-1"
              dynamic "nonsense_block" {
                for_each = []
                content {}
              }
            }"#;
        let d = diags_with(&schemas, src);
        let hit = d
            .iter()
            .find(|diag| {
                diag.message.contains("unknown nested block")
                    && diag.message.contains("nonsense_block")
            })
            .expect("unknown-label diagnostic");
        assert_eq!(hit.severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn well_formed_dynamic_yields_no_diagnostic() {
        let schemas = schemas_with_ebs_block_device();
        let src = r#"resource "aws_instance" "x" {
              ami = "ami-1"
              dynamic "ebs_block_device" {
                for_each = []
                content {
                  device_name = "/dev/sda1"
                  volume_size = 20
                }
              }
            }"#;
        let d = diags_with(&schemas, src);
        // No diagnostics should mention `dynamic`, `for_each`,
        // `device_name`, or `nonsense_block`.
        assert!(
            d.iter().all(|diag| !diag.message.contains("dynamic")
                && !diag.message.contains("for_each")
                && !diag.message.contains("device_name")),
            "well-formed dynamic produced spurious diagnostics: {d:?}"
        );
    }

    #[test]
    fn dynamic_does_not_leak_as_unknown_nested_block_on_resource() {
        // Before this commit, the `_ => { … }` catch-all silently
        // allowed `dynamic` at resource depth. Now it's a
        // recognised meta-block; make sure we didn't accidentally
        // start flagging it as an unknown attribute / block either.
        let schemas = schemas_with_ebs_block_device();
        let src = r#"resource "aws_instance" "x" {
              ami = "ami-1"
              dynamic "ebs_block_device" {
                for_each = []
                content {
                  device_name = "x"
                }
              }
            }"#;
        let d = diags_with(&schemas, src);
        assert!(
            !has_unknown(&d, "dynamic"),
            "dynamic misflagged as unknown attribute: {d:?}"
        );
    }
}
