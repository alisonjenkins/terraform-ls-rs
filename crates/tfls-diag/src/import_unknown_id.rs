//! `terraform_import_unknown_id` — flag an `import` block (Terraform 1.5+)
//! whose `id`, or whose `for_each` membership (Terraform 1.7+), depends on a
//! value not known until apply. Terraform rejects this at *plan* time:
//!
//! ```text
//! The import block "id" argument depends on resource attributes that
//! cannot be determined until apply.
//! ```
//!
//! Reuses the [`crate::unknown_value`] analysis: `id` is a scalar position
//! (the whole value must be plan-known), `for_each` follows the same
//! membership rule as the resource-level meta-argument. `each.*` is a safe
//! root, so an `id` derived from the block's own `for_each` iteration is
//! fine as long as that `for_each` is.

use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;

use crate::schema_validation::SchemaLookup;
use crate::unknown_value::{
    collect_module_inputs, expr_range, membership_apply_time, value_apply_time, MetaKind,
    ModuleOutputLookup, ModuleUnknownInputs, UnknownCtx,
};

/// Single-body entry point: resolves module inputs only within `body`.
pub fn import_unknown_id_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    import_unknown_id_diagnostics_with_ctx(body, rope, &ModuleUnknownInputs::default(), None, None)
}

/// Module-aware entry point; see
/// [`crate::for_each_unknown_keys_diagnostics_with_ctx`] for the
/// `module_inputs` / `schema` contract.
pub fn import_unknown_id_diagnostics_with_ctx(
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
        if block.ident.as_str() != "import" {
            continue;
        }
        for entry in block.body.iter() {
            let Some(attr) = entry.as_attribute() else {
                continue;
            };
            let hit = match attr.key.as_str() {
                "id" => value_apply_time(&attr.value, &ctx).then_some(
                    "`id` of this import block depends on a value not known until apply — \
                     Terraform requires import IDs to be known at plan time. Reference \
                     statically-known values (variables, config-set attributes) instead of \
                     computed resource attributes.",
                ),
                "for_each" => membership_apply_time(&attr.value, MetaKind::ForEach, &ctx)
                    .then_some(
                        "`for_each` membership of this import block depends on a value not \
                         known until apply — Terraform rejects this at plan time. Key it on \
                         a statically-known attribute instead.",
                    ),
                _ => None,
            };
            if let Some(message) = hit {
                out.push(Diagnostic {
                    range: expr_range(&attr.value, rope),
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("terraform-ls-rs".to_string()),
                    message: message.to_string(),
                    ..Default::default()
                });
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        import_unknown_id_diagnostics(&body, &rope)
    }

    fn flagged(src: &str) -> bool {
        !diags(src).is_empty()
    }

    #[test]
    fn silent_for_literal_id() {
        let src = r#"
import {
  to = aws_s3_bucket.b
  id = "my-bucket"
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn silent_for_var_id() {
        let src = r#"
import {
  to = aws_s3_bucket.b
  id = var.bucket_name
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn silent_for_each_value_id() {
        // id derived from the import block's own for_each over a variable.
        let src = r#"
import {
  for_each = var.buckets
  to       = aws_s3_bucket.b[each.key]
  id       = each.value
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_computed_resource_attr_id() {
        let src = r#"
import {
  to = aws_s3_bucket.b
  id = aws_instance.web.id
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn flags_id_via_local_chain() {
        let src = r#"
locals {
  bucket_id = aws_s3_bucket.source.id
}
import {
  to = aws_s3_bucket.b
  id = local.bucket_id
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_config_set_attr_id() {
        // The referenced attribute is set in config to a variable — the
        // shared resolution applies here too.
        let src = r#"
resource "aws_s3_bucket" "source" {
  bucket = var.name
}
import {
  to = aws_s3_bucket.b
  id = aws_s3_bucket.source.bucket
}
"#;
        assert!(!flagged(src), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_for_each_over_resource_derived_map() {
        let src = r#"
import {
  for_each = { for s in aws_subnet.all : s.id => s.arn }
  to       = aws_subnet.imported[each.key]
  id       = each.value
}
"#;
        assert!(flagged(src));
    }

    #[test]
    fn silent_for_import_without_id() {
        let src = r#"
import {
  to = aws_s3_bucket.b
}
"#;
        assert!(!flagged(src));
    }

    #[test]
    fn ignores_non_import_blocks() {
        let src = r#"
resource "null_resource" "x" {
  id = aws_instance.web.id
}
"#;
        assert!(!flagged(src));
    }
}
