//! `terraform_missing_tags` + `terraform_missing_name_tag` — warn when
//! taggable resources are left untagged.
//!
//! - `missing_tags_diagnostics`: schema-driven, provider-agnostic. A
//!   `resource` whose schema declares a `tags` (AWS/Azure) or `labels`
//!   (GCP/Kubernetes) attribute, but the block sets neither, gets a
//!   WARNING. Suppressed for any provider whose `provider` block declares
//!   `default_tags` (those tags auto-apply, so the resource isn't really
//!   untagged).
//! - `missing_name_tag_diagnostics`: AWS-specific, schema-free. A
//!   `resource` of a curated console-visible type (see
//!   [`AWS_NAME_TAG_RESOURCES`]) that has no statically-visible literal
//!   `Name` tag key gets a WARNING — those resources show up in the AWS
//!   console with useless names otherwise.
//!
//! Both default-on at WARNING; off-able / re-severitied via the per-rule
//! `rules` config (`terraform_missing_tags`, `terraform_missing_name_tag`).

use std::collections::HashSet;

use hcl_edit::expr::{Expression, ObjectKey};
use hcl_edit::repr::Span as _;
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

use crate::expr_walk::for_each_expression_in;
use crate::schema_validation::SchemaLookup;

/// AWS resource types whose `Name` tag drives the name shown in the AWS
/// console. Curated rather than schema-derived — "shows a Name column in
/// the console" is AWS-console knowledge the provider schema doesn't
/// encode. Keep sorted; extend as needed.
const AWS_NAME_TAG_RESOURCES: &[&str] = &[
    "aws_autoscaling_group",
    "aws_customer_gateway",
    "aws_db_instance",
    "aws_ebs_volume",
    "aws_eip",
    "aws_elb",
    "aws_instance",
    "aws_internet_gateway",
    "aws_lb",
    "aws_nat_gateway",
    "aws_network_interface",
    "aws_route_table",
    "aws_security_group",
    "aws_subnet",
    "aws_vpc",
    "aws_vpc_peering_connection",
    "aws_vpn_connection",
    "aws_vpn_gateway",
];

/// Warn on `resource` blocks whose schema declares a `tags`/`labels`
/// attribute that the block never sets. `suppressed_providers` holds the
/// local names (e.g. `aws`) of providers that declare `default_tags` —
/// resources of those providers are skipped.
pub fn missing_tags_diagnostics<L: SchemaLookup>(
    body: &Body,
    rope: &Rope,
    lookup: &L,
    suppressed_providers: &HashSet<String>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        // Tags on `data` sources are read-only output, not authoring
        // intent — only flag managed `resource` blocks.
        if block.ident.as_str() != "resource" {
            continue;
        }
        let Some(type_name) = first_label(block) else {
            continue;
        };
        let Some(schema) = lookup.resource(type_name) else {
            // No schema (provider not fetched) — stay silent, same as
            // schema_validation.
            continue;
        };
        // Pick whichever tag-like attribute the schema actually declares.
        let tag_attr = if schema.block.attributes.contains_key("tags") {
            "tags"
        } else if schema.block.attributes.contains_key("labels") {
            "labels"
        } else {
            continue; // not taggable
        };
        if suppressed_providers.contains(provider_of(type_name)) {
            continue;
        }
        if !block_has_attr(block, tag_attr) {
            push(
                &mut out,
                rope,
                anchor_range(block),
                format!("resource `{type_name}` supports `{tag_attr}` but none are set"),
            );
        }
    }
    out
}

/// Warn on curated AWS `resource` blocks that lack a statically-visible
/// literal `Name` tag key. Schema-free — works even before
/// `.terraform/providers` is fetched.
pub fn missing_name_tag_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "resource" {
            continue;
        }
        let Some(type_name) = first_label(block) else {
            continue;
        };
        if AWS_NAME_TAG_RESOURCES.binary_search(&type_name).is_err() {
            continue;
        }
        let has_name = block
            .body
            .iter()
            .filter_map(|s| s.as_attribute())
            .find(|a| a.key.as_str() == "tags")
            .is_some_and(|attr| expr_has_name_key(&attr.value));
        if !has_name {
            push(
                &mut out,
                rope,
                anchor_range(block),
                format!(
                    "resource `{type_name}` should set a `Name` tag (shown in the AWS console)"
                ),
            );
        }
    }
    out
}

/// Local names of `provider "<name>" { ... }` blocks that declare
/// `default_tags` (as a nested block or an attribute). Aggregated by the
/// LSP layer across module siblings to suppress `missing_tags`.
pub fn provider_names_with_default_tags(body: &Body) -> Vec<String> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "provider" {
            continue;
        }
        let Some(name) = first_label(block) else {
            continue;
        };
        let has_default_tags = block.body.iter().any(|s| {
            s.as_block()
                .is_some_and(|b| b.ident.as_str() == "default_tags")
                || s.as_attribute()
                    .is_some_and(|a| a.key.as_str() == "default_tags")
        });
        if has_default_tags {
            out.push(name.to_string());
        }
    }
    out
}

/// `true` if `expr` contains, anywhere in its tree, an object entry whose
/// key is the literal `Name`. Catches `{ Name = x }` directly and inside
/// `merge(common, { Name = x })`; `var.tags` / opaque funcs yield `false`.
fn expr_has_name_key(expr: &Expression) -> bool {
    let mut found = false;
    for_each_expression_in(expr, |e| {
        if let Expression::Object(obj) = e {
            for (key, _) in obj.iter() {
                if object_key_name(key) == Some("Name") {
                    found = true;
                }
            }
        }
    });
    found
}

fn object_key_name(k: &ObjectKey) -> Option<&str> {
    match k {
        ObjectKey::Ident(id) => Some(id.as_str()),
        ObjectKey::Expression(Expression::Variable(var)) => Some(var.value().as_str()),
        ObjectKey::Expression(Expression::String(s)) => Some(s.value().as_str()),
        _ => None,
    }
}

fn block_has_attr(block: &Block, name: &str) -> bool {
    block
        .body
        .iter()
        .filter_map(|s| s.as_attribute())
        .any(|a| a.key.as_str() == name)
}

/// Provider local name a resource type belongs to: the prefix before the
/// first `_` (`aws_instance` → `aws`). Aliased providers are an accepted
/// edge.
fn provider_of(type_name: &str) -> &str {
    type_name.split_once('_').map_or(type_name, |(p, _)| p)
}

fn first_label(block: &Block) -> Option<&str> {
    block.labels.first().map(|l| match l {
        BlockLabel::String(s) => s.value().as_str(),
        BlockLabel::Ident(i) => i.as_str(),
    })
}

/// Anchor the squiggle on the type-name label (`"aws_instance"`), falling
/// back to the block keyword if the label has no span.
fn anchor_range(block: &Block) -> Option<std::ops::Range<usize>> {
    block
        .labels
        .first()
        .and_then(|l| match l {
            BlockLabel::String(s) => s.span(),
            BlockLabel::Ident(i) => i.span(),
        })
        .or_else(|| block.ident.span())
}

fn push(
    out: &mut Vec<Diagnostic>,
    rope: &Rope,
    span: Option<std::ops::Range<usize>>,
    message: String,
) {
    let range = hcl_span_to_lsp_range(rope, span.unwrap_or(0..0)).unwrap_or_default();
    out.push(Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("terraform-ls-rs".to_string()),
        message,
        ..Default::default()
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tfls_parser::parse_source;
    use tfls_schema::{AttributeSchema, BlockSchema, Schema};

    /// Mock `SchemaLookup` driven by an attribute-name set per resource type.
    struct MockLookup {
        resources: HashMap<&'static str, Vec<&'static str>>,
    }

    impl MockLookup {
        fn new(entries: &[(&'static str, &[&'static str])]) -> Self {
            let resources = entries
                .iter()
                .map(|(t, attrs)| (*t, attrs.to_vec()))
                .collect();
            Self { resources }
        }
        fn schema_with(attrs: &[&str]) -> Schema {
            let attributes = attrs
                .iter()
                .map(|a| {
                    (
                        (*a).to_string(),
                        AttributeSchema {
                            optional: true,
                            ..Default::default()
                        },
                    )
                })
                .collect();
            Schema {
                version: 0,
                block: BlockSchema {
                    attributes,
                    ..Default::default()
                },
            }
        }
    }

    impl SchemaLookup for MockLookup {
        fn resource(&self, type_name: &str) -> Option<Schema> {
            self.resources
                .get(type_name)
                .map(|attrs| Self::schema_with(attrs))
        }
        fn data_source(&self, _type_name: &str) -> Option<Schema> {
            None
        }
    }

    fn parse(src: &str) -> (Body, Rope) {
        let body = parse_source(src).body.expect("parses");
        let rope = Rope::from_str(src);
        (body, rope)
    }

    fn generic(src: &str, lookup: &MockLookup, suppressed: &[&str]) -> Vec<Diagnostic> {
        let (body, rope) = parse(src);
        let suppressed: HashSet<String> = suppressed.iter().map(|s| s.to_string()).collect();
        missing_tags_diagnostics(&body, &rope, lookup, &suppressed)
    }

    fn name(src: &str) -> Vec<Diagnostic> {
        let (body, rope) = parse(src);
        missing_name_tag_diagnostics(&body, &rope)
    }

    // ---- generic missing-tags ----

    #[test]
    fn generic_flags_taggable_resource_without_tags() {
        let lookup = MockLookup::new(&[("aws_instance", &["ami", "tags"])]);
        let d = generic(r#"resource "aws_instance" "web" {}"#, &lookup, &[]);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("`tags`"));
    }

    #[test]
    fn generic_silent_when_tags_present_even_if_empty() {
        let lookup = MockLookup::new(&[("aws_instance", &["tags"])]);
        let d = generic(
            "resource \"aws_instance\" \"web\" {\n  tags = {}\n}",
            &lookup,
            &[],
        );
        assert!(d.is_empty(), "presence-only: got {d:?}");
    }

    #[test]
    fn generic_flags_missing_labels() {
        let lookup = MockLookup::new(&[("google_storage_bucket", &["name", "labels"])]);
        let d = generic(r#"resource "google_storage_bucket" "b" {}"#, &lookup, &[]);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("`labels`"));
    }

    #[test]
    fn generic_silent_for_data_source() {
        let lookup = MockLookup::new(&[("aws_instance", &["tags"])]);
        let d = generic(r#"data "aws_instance" "web" {}"#, &lookup, &[]);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn generic_silent_when_not_taggable() {
        let lookup = MockLookup::new(&[("aws_iam_policy", &["name", "policy"])]);
        let d = generic(r#"resource "aws_iam_policy" "p" {}"#, &lookup, &[]);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn generic_silent_on_schema_miss() {
        let lookup = MockLookup::new(&[]);
        let d = generic(r#"resource "aws_instance" "web" {}"#, &lookup, &[]);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn generic_suppressed_by_default_tags_provider() {
        let lookup = MockLookup::new(&[("aws_instance", &["tags"])]);
        let d = generic(r#"resource "aws_instance" "web" {}"#, &lookup, &["aws"]);
        assert!(d.is_empty(), "got: {d:?}");
    }

    // ---- AWS Name-tag ----

    #[test]
    fn name_flags_curated_resource_without_tags() {
        let d = name(r#"resource "aws_instance" "web" {}"#);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("Name"));
    }

    #[test]
    fn name_silent_when_literal_name_present() {
        let d = name("resource \"aws_instance\" \"web\" {\n  tags = { Name = \"x\" }\n}");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn name_flags_tags_without_name_key() {
        let d = name("resource \"aws_instance\" \"web\" {\n  tags = { Env = \"prod\" }\n}");
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn name_silent_when_name_inside_merge() {
        let d = name(
            "resource \"aws_instance\" \"web\" {\n  tags = merge(local.common, { Name = \"x\" })\n}",
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn name_flags_opaque_var_tags() {
        let d = name("resource \"aws_instance\" \"web\" {\n  tags = var.tags\n}");
        assert_eq!(d.len(), 1, "got: {d:?}");
    }

    #[test]
    fn name_silent_for_non_curated_resource() {
        let d = name(r#"resource "aws_s3_bucket" "b" {}"#);
        assert!(d.is_empty(), "got: {d:?}");
    }

    // ---- default_tags extraction ----

    #[test]
    fn detects_provider_default_tags_block() {
        let (body, _) =
            parse("provider \"aws\" {\n  default_tags {\n    tags = { Team = \"x\" }\n  }\n}");
        assert_eq!(provider_names_with_default_tags(&body), vec!["aws"]);
    }

    #[test]
    fn no_default_tags_for_plain_provider() {
        let (body, _) = parse("provider \"aws\" {\n  region = \"us-east-1\"\n}");
        assert!(provider_names_with_default_tags(&body).is_empty());
    }
}
