//! `terraform_for_each_unknown_keys` — flag a `for_each` / `count` whose
//! **set of keys** (which elements exist) depends on a value not known until
//! apply. Terraform / OpenTofu only errors on this at *plan* time:
//!
//! ```text
//! Invalid for_each argument: ... includes keys or set values from resource
//! attributes that cannot be determined until apply.
//! ```
//!
//! The bug class: the *membership* of the `for_each` map (which keys exist),
//! or the set elements, is derived from a managed-resource attribute that
//! itself depends on a not-yet-created resource. Values being unknown is fine
//! — only the **keys**, the **set elements**, and the **`if` predicate** that
//! filters membership matter.
//!
//! The expression analysis lives in [`crate::unknown_value`]; this module is
//! the rule driver (which blocks / attributes to check, and the messages).
//!
//! ## Scope / limitations
//!
//! - Module-aware: `local.*` definitions are resolved across sibling files
//!   when the caller threads [`ModuleUnknownInputs`] (the LSP layer does);
//!   the single-body entry point sees only the active file.
//! - Top-level `resource` / `data` / `module` blocks only (not `dynamic`).
//! - Over-approximates when a loop value is used *whole* (no field access):
//!   conservatively treats it as apply-time if the collection embeds any
//!   resource attribute.

use std::collections::HashMap;

use hcl_edit::expr::Expression;
use hcl_edit::structure::{Block, Body};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;

use crate::schema_validation::SchemaLookup;
use crate::unknown_value::{
    collect_module_inputs, expr_range, membership_apply_time, MetaKind, ModuleOutputLookup,
    ModuleUnknownInputs, UnknownCtx,
};

pub use crate::unknown_value::collect_locals;

/// Single-body entry point: resolves `local.*` only within `body`. Kept for
/// tests and callers without module context.
pub fn for_each_unknown_keys_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    for_each_unknown_keys_diagnostics_with_locals(body, rope, &HashMap::new())
}

/// Module-aware entry point over `local.*` definitions only. Thin form of
/// [`for_each_unknown_keys_diagnostics_with_ctx`] kept for callers (and
/// tests) that don't aggregate resource / data configs.
pub fn for_each_unknown_keys_diagnostics_with_locals(
    body: &Body,
    rope: &Rope,
    module_locals: &HashMap<String, Expression>,
) -> Vec<Diagnostic> {
    let inputs = ModuleUnknownInputs {
        locals: module_locals.clone(),
        ..Default::default()
    };
    for_each_unknown_keys_diagnostics_with_ctx(body, rope, &inputs, None, None)
}

/// Primary module-aware entry point: `module_inputs` carries the `local.*`
/// definitions and `resource` / `data` block configs aggregated across every
/// `.tf` file in the active module's directory (a `locals` block typically
/// lives in a different file from the `for_each` that reads it). `body`'s own
/// definitions are overlaid on top so they always resolve even if `body` is
/// not yet present in the aggregated set. `schema`, when present, refines
/// resource-attribute classification (a non-computed attribute is plan-known).
pub fn for_each_unknown_keys_diagnostics_with_ctx(
    body: &Body,
    rope: &Rope,
    module_inputs: &ModuleUnknownInputs,
    schema: Option<&dyn SchemaLookup>,
    module_outputs: Option<&dyn ModuleOutputLookup>,
) -> Vec<Diagnostic> {
    let mut inputs = module_inputs.clone();
    inputs.merge_override(collect_module_inputs(body));
    let ctx = UnknownCtx::new(&inputs, schema).with_module_outputs(module_outputs);
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if !matches!(block.ident.as_str(), "resource" | "data" | "module") {
            continue;
        }
        check_block(block, rope, &ctx, &mut out);
    }
    out
}

fn check_block(block: &Block, rope: &Rope, ctx: &UnknownCtx, out: &mut Vec<Diagnostic>) {
    for entry in block.body.iter() {
        let Some(attr) = entry.as_attribute() else {
            continue;
        };
        let kind = match attr.key.as_str() {
            "for_each" => MetaKind::ForEach,
            "count" => MetaKind::Count,
            _ => continue,
        };
        if membership_apply_time(&attr.value, kind, ctx) {
            let range = expr_range(&attr.value, rope);
            let mut message = message(kind).to_string();
            // When the unknownness comes from a caller-passed variable,
            // name the caller — the fix usually lives in the other module.
            if let Some(reason) = crate::unknown_value::unknown_var_reason(&attr.value, ctx) {
                message = format!("{message} ({reason}.)");
            }
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message,
                ..Default::default()
            });
        }
    }
}

fn message(kind: MetaKind) -> &'static str {
    match kind {
        MetaKind::ForEach => {
            "`for_each` membership depends on a value not known until apply — \
             Terraform rejects this at plan time. Key it on a statically-known \
             attribute instead (values may stay unknown; only the keys must be known)."
        }
        MetaKind::Count => {
            "`count` depends on a value not known until apply — Terraform rejects \
             this at plan time. Base the count on a statically-known value instead."
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        for_each_unknown_keys_diagnostics(&body, &rope)
    }

    fn flagged(src: &str) -> bool {
        !diags(src).is_empty()
    }

    const BROKEN: &str = r#"
resource "aws_iam_role" "r" {
  name               = "example"
  assume_role_policy = "{}"
}

locals {
  items = {
    "a" = { policy = aws_iam_role.r.arn }
    "b" = {}
  }
}

resource "null_resource" "broken" {
  for_each = { for k, v in local.items : k => v if lookup(v, "policy", "") != "" }
}
"#;

    const FIXED: &str = r#"
resource "aws_iam_role" "r" {
  name               = "example"
  assume_role_policy = "{}"
}

locals {
  items_fixed = {
    "a" = { has_policy = true, policy = aws_iam_role.r.arn }
    "b" = {}
  }
}

resource "null_resource" "fixed" {
  for_each = { for k, v in local.items_fixed : k => v if lookup(v, "has_policy", false) }
}
"#;

    #[test]
    fn flags_broken_minimal_recreation() {
        assert!(flagged(BROKEN), "broken for_each should be flagged");
    }

    #[test]
    fn silent_for_fixed_static_filter() {
        let d = diags(FIXED);
        assert!(d.is_empty(), "fixed for_each should be silent; got: {d:?}");
    }

    #[test]
    fn flags_canonical_resource_id_keys() {
        // `for_each = { for s in aws_subnet.all : s.id => s }` — keys are
        // resource IDs, unknown until apply.
        let src = r#"
resource "null_resource" "x" {
  for_each = { for s in aws_subnet.all : s.id => s }
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_input_var_id_keys() {
        // Same shape but over an input variable — keys are plan-known.
        let src = r#"
resource "null_resource" "x" {
  for_each = { for s in var.subnets : s.id => s }
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_toset_of_resource_ids() {
        let src = r#"
resource "null_resource" "x" {
  for_each = toset([for s in aws_subnet.all : s.id])
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_plain_var_map() {
        let src = r#"
resource "null_resource" "x" {
  for_each = var.instances
}
"#;
        assert!(!flagged(src));
    }

    #[test]
    fn flags_plain_resource_reference() {
        let src = r#"
resource "null_resource" "x" {
  for_each = aws_subnet.all
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_unresolved_data_source_in_filter() {
        // A data source is read during plan — its attributes are plan-known
        // in the normal case. With the block unresolvable (not declared in
        // this module view) we stay silent rather than guess.
        let src = r#"
resource "null_resource" "x" {
  for_each = { for k, v in var.items : k => v if data.aws_iam_role.r.arn != "" }
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn silent_for_data_source_with_static_config() {
        let src = r#"
data "aws_subnets" "all" {
  filter {
    name   = "vpc-id"
    values = [var.vpc_id]
  }
}
resource "null_resource" "x" {
  for_each = toset(data.aws_subnets.all.ids)
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_data_source_with_depends_on() {
        // depends_on on a managed resource defers the data read to apply —
        // its attributes become unknown at plan.
        let src = r#"
data "aws_subnets" "all" {
  depends_on = [aws_vpc.main]
  filter {
    name   = "vpc-id"
    values = [var.vpc_id]
  }
}
resource "null_resource" "x" {
  for_each = toset(data.aws_subnets.all.ids)
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_data_source_with_data_depends_on() {
        // depends_on on another data source / module does not defer the read
        // the way a managed-resource target does.
        let src = r#"
data "aws_subnets" "all" {
  depends_on = [data.aws_vpc.main]
}
resource "null_resource" "x" {
  for_each = toset(data.aws_subnets.all.ids)
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_data_source_with_apply_time_config() {
        // The data source's own config references a managed-resource
        // attribute — read deferred, attributes unknown. The reference here
        // sits in a nested filter block.
        let src = r#"
data "aws_subnets" "all" {
  filter {
    name   = "vpc-id"
    values = [aws_vpc.main.id]
  }
}
resource "null_resource" "x" {
  for_each = toset(data.aws_subnets.all.ids)
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn flags_data_chain_through_deferred_data() {
        // data.b is plan-known on its own, but reads data.a which is
        // deferred — the unknownness propagates through the chain.
        let src = r#"
data "aws_vpc" "a" {
  depends_on = [aws_internet_gateway.gw]
}
data "aws_subnets" "b" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.a.id]
  }
}
resource "null_resource" "x" {
  for_each = toset(data.aws_subnets.b.ids)
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_resource_attr_set_in_config() {
        // The referenced attribute is set explicitly in the resource's
        // config to a plan-known expression — its value is plan-known.
        let src = r#"
resource "aws_s3_bucket" "b" {
  bucket = var.bucket_name
}
resource "null_resource" "x" {
  for_each = toset([aws_s3_bucket.b.bucket])
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_resource_attr_set_to_apply_time_config() {
        // Set in config, but to another resource's computed attribute —
        // unknownness propagates through the chain.
        let src = r#"
resource "aws_s3_bucket" "b" {
  bucket = aws_vpc.main.id
}
resource "null_resource" "x" {
  for_each = toset([aws_s3_bucket.b.bucket])
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn flags_resource_attr_not_in_config() {
        // Declared block, but the referenced attribute is absent from config
        // — computed, unknown until apply. Pins the status quo.
        let src = r#"
resource "aws_s3_bucket" "b" {
  bucket = var.bucket_name
}
resource "null_resource" "x" {
  for_each = toset([aws_s3_bucket.b.arn])
}
"#;
        assert!(flagged(src));
    }

    /// Mock schema: `aws_s3_bucket` with `bucket` non-computed and `arn`
    /// computed.
    struct MockSchemas;

    impl crate::schema_validation::SchemaLookup for MockSchemas {
        fn resource(&self, type_name: &str) -> Option<tfls_schema::Schema> {
            if type_name != "aws_s3_bucket" {
                return None;
            }
            let mut block = tfls_schema::BlockSchema::default();
            block.attributes.insert(
                "bucket".to_string(),
                tfls_schema::AttributeSchema {
                    optional: true,
                    ..Default::default()
                },
            );
            block.attributes.insert(
                "arn".to_string(),
                tfls_schema::AttributeSchema {
                    computed: true,
                    ..Default::default()
                },
            );
            Some(tfls_schema::Schema { version: 0, block })
        }
        fn data_source(&self, _type_name: &str) -> Option<tfls_schema::Schema> {
            None
        }
    }

    fn diags_with_schema(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        for_each_unknown_keys_diagnostics_with_ctx(
            &body,
            &rope,
            &ModuleUnknownInputs::default(),
            Some(&MockSchemas),
            None,
        )
    }

    /// Mock module-output lookup: `module.net.subnet_ids` apply-time,
    /// `module.net.cidr` plan-known, everything else unresolvable.
    struct MockOutputs;

    impl ModuleOutputLookup for MockOutputs {
        fn output_apply_time(
            &self,
            module_label: &str,
            output: &str,
            _rest: &[&str],
        ) -> Option<bool> {
            match (module_label, output) {
                ("net", "subnet_ids") => Some(true),
                ("net", "cidr") => Some(false),
                _ => None,
            }
        }
    }

    fn diags_with_outputs(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        for_each_unknown_keys_diagnostics_with_ctx(
            &body,
            &rope,
            &ModuleUnknownInputs::default(),
            None,
            Some(&MockOutputs),
        )
    }

    #[test]
    fn flags_apply_time_module_output() {
        let src = r#"
resource "null_resource" "x" {
  for_each = toset(module.net.subnet_ids)
}
"#;
        assert!(!diags_with_outputs(src).is_empty());
    }

    #[test]
    fn silent_for_plan_known_module_output() {
        let src = r#"
resource "null_resource" "x" {
  for_each = toset([module.net.cidr])
}
"#;
        let d = diags_with_outputs(src);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_unresolvable_module_output() {
        let src = r#"
resource "null_resource" "x" {
  for_each = toset(module.unknown.ids)
}
"#;
        let d = diags_with_outputs(src);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_module_output_without_lookup() {
        // No lookup wired (non-LSP callers): every module output is treated
        // plan-known — pins the pre-existing behaviour.
        let src = r#"
resource "null_resource" "x" {
  for_each = toset(module.net.subnet_ids)
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn silent_for_bare_module_reference() {
        let src = r#"
resource "null_resource" "x" {
  for_each = module.net
}
"#;
        let d = diags_with_outputs(src);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn schema_silences_non_computed_attr_of_unseen_block() {
        // No declared block in view, but the schema says `bucket` is not
        // computed — its value can only come from config (or null), so it is
        // plan-known.
        let src = r#"
resource "null_resource" "x" {
  for_each = toset([aws_s3_bucket.unseen.bucket])
}
"#;
        assert!(flagged(src), "without schema: conservative default");
        let d = diags_with_schema(src);
        assert!(d.is_empty(), "with schema: non-computed is plan-known; got: {d:?}");
    }

    #[test]
    fn schema_keeps_flagging_computed_attr() {
        let src = r#"
resource "null_resource" "x" {
  for_each = toset([aws_s3_bucket.unseen.arn])
}
"#;
        assert!(!diags_with_schema(src).is_empty());
    }

    #[test]
    fn schema_config_set_apply_time_attr_still_flags() {
        // The attr is non-computed per schema, but config sets it to an
        // apply-time expression — the config resolution must win over the
        // schema shortcut.
        let src = r#"
resource "aws_s3_bucket" "b" {
  bucket = aws_vpc.main.id
}
resource "null_resource" "x" {
  for_each = toset([aws_s3_bucket.b.bucket])
}
"#;
        assert!(!diags_with_schema(src).is_empty());
    }

    #[test]
    fn resource_reference_cycle_terminates() {
        // Mutually-referencing resource configs (invalid Terraform, but must
        // not hang). Cycle keeps the conservative default (flag).
        let src = r#"
resource "null_resource" "a" {
  triggers = { v = null_resource.b.triggers.v }
}
resource "null_resource" "b" {
  triggers = { v = null_resource.a.triggers.v }
}
resource "null_resource" "x" {
  for_each = toset([null_resource.a.triggers.v])
}
"#;
        let _ = diags(src);
    }

    #[test]
    fn data_reference_cycle_terminates() {
        // Mutually-referencing data sources (invalid config, but must not
        // hang). Cycle resolves to plan-known.
        let src = r#"
data "aws_vpc" "a" {
  id = data.aws_vpc.b.id
}
data "aws_vpc" "b" {
  id = data.aws_vpc.a.id
}
resource "null_resource" "x" {
  for_each = toset([data.aws_vpc.a.id])
}
"#;
        let _ = diags(src);
    }

    #[test]
    fn flags_count_length_of_resource_comprehension() {
        let src = r#"
resource "null_resource" "x" {
  count = length([for s in aws_subnet.all : s.id])
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_count_length_of_var() {
        let src = r#"
resource "null_resource" "x" {
  count = length(var.list)
}
"#;
        assert!(!flagged(src));
    }

    #[test]
    fn silent_for_count_conditional() {
        let src = r#"
resource "null_resource" "x" {
  count = var.enabled ? 1 : 0
}
"#;
        assert!(!flagged(src));
    }

    #[test]
    fn silent_for_static_local_filter() {
        // Filter reads a fully-static local — no resource refs anywhere.
        let src = r#"
locals {
  items = {
    "a" = { enabled = true }
    "b" = { enabled = false }
  }
}
resource "null_resource" "x" {
  for_each = { for k, v in local.items : k => v if v.enabled }
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn cyclic_locals_terminate() {
        // Self-referential locals must not loop forever.
        let src = r#"
locals {
  a = local.b
  b = local.a
}
resource "null_resource" "x" {
  for_each = local.a
}
"#;
        // Just assert it returns (no hang / no panic).
        let _ = diags(src);
    }

    #[test]
    fn flags_field_access_via_dot() {
        // `v.policy` (dot access, not lookup) on an apply-time field.
        let src = r#"
locals {
  items = {
    "a" = { policy = aws_iam_role.r.arn }
  }
}
resource "null_resource" "x" {
  for_each = { for k, v in local.items : k => v if v.policy != "" }
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn resolves_locals_from_sibling_file() {
        // The `locals` block and the resource it embeds live in a *different*
        // file than the `for_each` that reads them — the single-body path
        // can't see this; the module-aware path must.
        let sibling = r#"
resource "aws_iam_role" "r" {
  name               = "x"
  assume_role_policy = "{}"
}
locals {
  items = {
    "a" = { policy = aws_iam_role.r.arn }
    "b" = {}
  }
}
"#;
        let main = r#"
resource "null_resource" "broken" {
  for_each = { for k, v in local.items : k => v if lookup(v, "policy", "") != "" }
}
"#;
        let sibling_body = parse_source(sibling).body.expect("parses");
        let module_locals = collect_locals(&sibling_body);
        let rope = Rope::from_str(main);
        let body = parse_source(main).body.expect("parses");

        // Single-body path is blind to the sibling local — no diagnostic.
        assert!(
            for_each_unknown_keys_diagnostics(&body, &rope).is_empty(),
            "single-body path should not resolve a sibling local"
        );
        // Module-aware path resolves it and flags.
        let d = for_each_unknown_keys_diagnostics_with_locals(&body, &rope, &module_locals);
        assert!(
            !d.is_empty(),
            "sibling-file local should resolve; got: {d:?}"
        );
    }

    const ACM_CANONICAL: &str = r#"
resource "aws_acm_certificate" "cert" {
  domain_name       = "example.com"
  validation_method = "DNS"
}

resource "aws_route53_record" "validation" {
  for_each = {
    for dvo in aws_acm_certificate.cert.domain_validation_options : dvo.domain_name => {
      name   = dvo.resource_record_name
      record = dvo.resource_record_value
      type   = dvo.resource_record_type
    }
  }

  name    = each.value.name
  records = [each.value.record]
  type    = each.value.type
  zone_id = var.zone_id
  ttl     = 60
}
"#;

    #[test]
    fn silent_for_acm_canonical_validation_pattern() {
        // The documented ACM DNS-validation pattern: the AWS provider
        // populates domain_validation_options at plan time (CustomizeDiff),
        // keyed fields (domain_name) are config-derived. Values stay unknown
        // — which is fine; only the keys must be plan-known.
        let d = diags(ACM_CANONICAL);
        assert!(d.is_empty(), "canonical ACM pattern is plan-valid; got: {d:?}");
    }

    #[test]
    fn flags_acm_keyed_on_record_name() {
        // Same collection, but keyed on an apply-time field — Terraform
        // rejects this at plan.
        let src = r#"
resource "aws_route53_record" "validation" {
  for_each = {
    for dvo in aws_acm_certificate.cert.domain_validation_options : dvo.resource_record_name => dvo
  }
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_acm_count_length() {
        // length(<plan-known-membership collection>) is plan-known even
        // though element values are not.
        let src = r#"
resource "null_resource" "x" {
  count = length(aws_acm_certificate.cert.domain_validation_options)
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn silent_for_acm_splat_known_field() {
        let src = r#"
resource "null_resource" "x" {
  for_each = toset(aws_acm_certificate.cert.domain_validation_options[*].domain_name)
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_acm_splat_unknown_field() {
        let src = r#"
resource "null_resource" "x" {
  for_each = toset(aws_acm_certificate.cert.domain_validation_options[*].resource_record_value)
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn flags_acm_bare_collection_for_each() {
        // Directly iterating the collection: the set elements are whole
        // objects carrying apply-time fields, so the key set is unknown
        // (and a set of objects is not a valid for_each anyway).
        let src = r#"
resource "null_resource" "x" {
  for_each = aws_acm_certificate.cert.domain_validation_options
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_acm_known_field_in_filter() {
        // Filter reads a plan-known field of the allowlisted collection.
        let src = r#"
resource "aws_route53_record" "validation" {
  for_each = {
    for dvo in aws_acm_certificate.cert.domain_validation_options :
      dvo.domain_name => dvo if dvo.domain_name != "ignored.example.com"
  }
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    fn diags_with_unknown_var(src: &str, name: &str, membership: bool, value: bool) -> Vec<Diagnostic> {
        use crate::unknown_value::UnknownVarInfo;
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let mut inputs = ModuleUnknownInputs::default();
        inputs.unknown_variables.insert(
            name.to_string(),
            UnknownVarInfo {
                membership,
                value,
                reason: "caller module \"net\" in /project passes an apply-time value"
                    .to_string(),
            },
        );
        for_each_unknown_keys_diagnostics_with_ctx(&body, &rope, &inputs, None, None)
    }

    const VAR_FOR_EACH: &str = r#"
resource "null_resource" "x" {
  for_each = var.subnets
}
"#;

    #[test]
    fn flags_caller_unknown_var_membership() {
        let d = diags_with_unknown_var(VAR_FOR_EACH, "subnets", true, true);
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(
            d[0].message.contains("caller module \"net\""),
            "message names the caller; got: {}",
            d[0].message
        );
    }

    #[test]
    fn silent_for_unknown_values_with_known_keys() {
        // Caller passes a map with static keys but apply-time values —
        // valid for_each, must stay silent.
        let d = diags_with_unknown_var(VAR_FOR_EACH, "subnets", false, true);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn silent_for_var_not_in_unknown_map() {
        let d = diags_with_unknown_var(VAR_FOR_EACH, "other_var", true, true);
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_count_length_of_caller_unknown_var() {
        let src = r#"
resource "null_resource" "x" {
  count = length(var.subnets)
}
"#;
        assert_eq!(diags_with_unknown_var(src, "subnets", true, true).len(), 1);
    }

    #[test]
    fn flags_toset_of_caller_unknown_var() {
        let src = r#"
resource "null_resource" "x" {
  for_each = toset(var.subnets)
}
"#;
        assert_eq!(diags_with_unknown_var(src, "subnets", true, true).len(), 1);
    }

    #[test]
    fn flags_value_bit_in_filter_condition() {
        // The var is used in the `if` predicate — a VALUE position deciding
        // membership.
        let src = r#"
resource "null_resource" "x" {
  for_each = { for k, v in var.items : k => v if var.flag }
}
"#;
        assert_eq!(diags_with_unknown_var(src, "flag", false, true).len(), 1);
    }

    #[test]
    fn flags_keys_of_caller_unknown_map() {
        // `for k, v in var.m : k => v` — keys come from the passed map's
        // membership.
        let src = r#"
resource "null_resource" "x" {
  for_each = { for k, v in var.m : k => v }
}
"#;
        assert_eq!(diags_with_unknown_var(src, "m", true, false).len(), 1);
        assert!(diags_with_unknown_var(src, "m", false, true).is_empty());
    }

    #[test]
    fn only_resource_data_module_blocks() {
        // A `for_each` in some other top-level block kind is ignored.
        let src = r#"
output "x" {
  value = { for s in aws_subnet.all : s.id => s }
}
"#;
        assert!(!flagged(src));
    }
}
