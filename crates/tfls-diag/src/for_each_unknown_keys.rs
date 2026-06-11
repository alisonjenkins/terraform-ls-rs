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
    collect_module_inputs, expr_range, membership_apply_time, MetaKind, ModuleUnknownInputs,
    UnknownCtx,
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
    for_each_unknown_keys_diagnostics_with_ctx(body, rope, &inputs, None)
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
) -> Vec<Diagnostic> {
    let mut inputs = module_inputs.clone();
    inputs.merge_override(collect_module_inputs(body));
    let ctx = UnknownCtx::new(&inputs, schema);
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
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message: message(kind).to_string(),
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
    fn flags_data_source_in_filter() {
        let src = r#"
resource "null_resource" "x" {
  for_each = { for k, v in var.items : k => v if data.aws_iam_role.r.arn != "" }
}
"#;
        assert!(flagged(src));
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
