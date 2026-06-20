//! `terraform_tftest` — structural validation for Terraform / OpenTofu test
//! files (`.tftest.hcl` / `.tftest.json`).
//!
//! The test grammar is small and CLOSED, so this flags shapes that aren't
//! part of it: unknown top-level or `run`-level blocks, missing required
//! attributes (`assert.condition` / `assert.error_message`,
//! `override_*.target`), and invalid enum values (`command`,
//! `plan_options.mode`, `override_during`). Module-configuration rules
//! (schema validation, unused declarations, deprecations, …) do not apply to
//! test files and are gated off by the caller; this is the only rule that
//! runs on them (besides syntax + undefined-reference).

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Block;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

const TOP_LEVEL_BLOCKS: &[&str] = &[
    "test",
    "variables",
    "provider",
    "run",
    "mock_provider",
    "override_resource",
    "override_data",
    "override_module",
];

/// Blocks allowed inside a `run { }`.
const RUN_BLOCKS: &[&str] = &[
    "plan_options",
    "variables",
    "module",
    "assert",
    "override_resource",
    "override_data",
    "override_module",
    "mock_provider",
];

/// Structural diagnostics for a parsed test-file body.
pub fn tftest_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        let name = block.ident.as_str();
        if !TOP_LEVEL_BLOCKS.contains(&name) {
            error(
                &mut out,
                ident_range(block, rope),
                format!(
                    "unknown test-file block `{name}` — expected one of: \
                     test, variables, provider, run, mock_provider, \
                     override_resource, override_data, override_module"
                ),
            );
            continue;
        }
        match name {
            "run" => validate_run(block, rope, &mut out),
            "override_resource" | "override_data" | "override_module" => {
                require_attr(block, "target", rope, &mut out);
                check_override_during(block, rope, &mut out);
            }
            "mock_provider" => validate_mock_provider(block, rope, &mut out),
            _ => {}
        }
    }
    out
}

fn validate_run(run: &Block, rope: &Rope, out: &mut Vec<Diagnostic>) {
    // command = plan|apply (bare keyword or string).
    if let Some(val) = attr_value(run, "command") {
        if let Some(kw) = keyword_or_string(val) {
            if kw != "plan" && kw != "apply" {
                error(
                    out,
                    expr_range(val, rope),
                    format!("invalid `command` `{kw}` — expected `plan` or `apply`"),
                );
            }
        }
    }
    for structure in run.body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        let name = block.ident.as_str();
        if !RUN_BLOCKS.contains(&name) {
            error(
                out,
                ident_range(block, rope),
                format!(
                    "unknown block `{name}` in `run` — expected one of: \
                     plan_options, variables, module, assert, \
                     override_resource, override_data, override_module, mock_provider"
                ),
            );
            continue;
        }
        match name {
            "assert" => {
                require_attr(block, "condition", rope, out);
                require_attr(block, "error_message", rope, out);
            }
            "plan_options" => validate_plan_options(block, rope, out),
            "override_resource" | "override_data" | "override_module" => {
                require_attr(block, "target", rope, out);
                check_override_during(block, rope, out);
            }
            _ => {}
        }
    }
}

fn validate_plan_options(block: &Block, rope: &Rope, out: &mut Vec<Diagnostic>) {
    if let Some(val) = attr_value(block, "mode") {
        if let Some(kw) = keyword_or_string(val) {
            if kw != "normal" && kw != "refresh-only" {
                error(
                    out,
                    expr_range(val, rope),
                    format!("invalid `mode` `{kw}` — expected `normal` or `refresh-only`"),
                );
            }
        }
    }
}

fn validate_mock_provider(block: &Block, rope: &Rope, out: &mut Vec<Diagnostic>) {
    check_override_during(block, rope, out);
    for structure in block.body.iter() {
        let Some(inner) = structure.as_block() else {
            continue;
        };
        let name = inner.ident.as_str();
        if name != "mock_resource" && name != "mock_data" {
            error(
                out,
                ident_range(inner, rope),
                format!(
                    "unknown block `{name}` in `mock_provider` — expected `mock_resource` or `mock_data`"
                ),
            );
        } else {
            check_override_during(inner, rope, out);
        }
    }
}

fn check_override_during(block: &Block, rope: &Rope, out: &mut Vec<Diagnostic>) {
    if let Some(val) = attr_value(block, "override_during") {
        if let Some(kw) = keyword_or_string(val) {
            if kw != "plan" && kw != "apply" {
                error(
                    out,
                    expr_range(val, rope),
                    format!("invalid `override_during` `{kw}` — expected `plan` or `apply`"),
                );
            }
        }
    }
}

fn require_attr(block: &Block, key: &str, rope: &Rope, out: &mut Vec<Diagnostic>) {
    if attr_value(block, key).is_none() {
        let label = block.ident.as_str();
        error(
            out,
            ident_range(block, rope),
            format!("`{label}` is missing required attribute `{key}`"),
        );
    }
}

fn attr_value<'a>(block: &'a Block, key: &str) -> Option<&'a Expression> {
    block.body.iter().find_map(|s| {
        s.as_attribute()
            .filter(|a| a.key.as_str() == key)
            .map(|a| &a.value)
    })
}

/// A bare keyword (`plan`) parses as a `Variable`; a quoted value
/// (`"refresh-only"`) as a `String`. Read either for enum checks.
fn keyword_or_string(e: &Expression) -> Option<&str> {
    match e {
        Expression::Variable(v) => Some(v.as_str()),
        Expression::String(s) => Some(s.value().as_str()),
        _ => None,
    }
}

fn ident_range(block: &Block, rope: &Rope) -> Range {
    block
        .ident
        .span()
        .and_then(|s| hcl_span_to_lsp_range(rope, s).ok())
        .unwrap_or_default()
}

fn expr_range(e: &Expression, rope: &Rope) -> Range {
    e.span()
        .and_then(|s| hcl_span_to_lsp_range(rope, s).ok())
        .unwrap_or_default()
}

fn error(out: &mut Vec<Diagnostic>, range: Range, message: String) {
    out.push(Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("terraform-ls-rs".to_string()),
        message,
        ..Default::default()
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        tftest_diagnostics(&body, &rope)
    }

    fn has(src: &str, needle: &str) -> bool {
        diags(src).iter().any(|d| d.message.contains(needle))
    }

    #[test]
    fn valid_test_file_is_clean() {
        let src = "variables {\n  region = \"eu\"\n}\nrun \"x\" {\n  command = plan\n  assert {\n    condition     = var.region != \"\"\n    error_message = \"region required\"\n  }\n}\n";
        assert!(diags(src).is_empty(), "got: {:?}", diags(src));
    }

    #[test]
    fn flags_unknown_top_level_block() {
        assert!(has("resource \"x\" \"y\" {}\n", "unknown test-file block `resource`"));
    }

    #[test]
    fn flags_unknown_block_in_run() {
        assert!(has(
            "run \"x\" {\n  lifecycle {}\n}\n",
            "unknown block `lifecycle` in `run`"
        ));
    }

    #[test]
    fn flags_bad_command_enum() {
        assert!(has("run \"x\" {\n  command = \"destroy\"\n}\n", "invalid `command`"));
        // bare keyword form is also checked
        assert!(has("run \"x\" {\n  command = destroy\n}\n", "invalid `command`"));
    }

    #[test]
    fn flags_assert_missing_required() {
        assert!(has(
            "run \"x\" {\n  assert {\n    condition = true\n  }\n}\n",
            "missing required attribute `error_message`"
        ));
        assert!(has(
            "run \"x\" {\n  assert {\n    error_message = \"m\"\n  }\n}\n",
            "missing required attribute `condition`"
        ));
    }

    #[test]
    fn flags_override_missing_target() {
        assert!(has(
            "override_resource {\n  values = {}\n}\n",
            "missing required attribute `target`"
        ));
    }

    #[test]
    fn flags_bad_plan_options_mode() {
        assert!(has(
            "run \"x\" {\n  plan_options {\n    mode = \"sideways\"\n  }\n}\n",
            "invalid `mode`"
        ));
        assert!(!has(
            "run \"x\" {\n  plan_options {\n    mode = \"refresh-only\"\n  }\n}\n",
            "invalid `mode`"
        ));
    }
}
