//! `terraform_duplicate_definition` — flag two definitions sharing the
//! same address within a single file.
//!
//! `terraform validate` errors hard on duplicate declarations (one of the
//! most common copy-paste mistakes), but the server otherwise stays
//! silent until the CLI runs. This catches the SAME-FILE case from a raw
//! body scan; the per-document `SymbolTable` already de-duplicates
//! same-file definitions, so the index can't see them — only a fresh walk
//! can. Cross-file duplicates (the same `variable "x"` in two files of one
//! module) are a separate, index-driven concern.
//!
//! Addresses checked:
//! - `variable` / `output` / `module` — keyed by name.
//! - `resource` / `data` — keyed by `(type, name)`.
//! - `locals { ... }` attribute names (aggregated across every `locals`
//!   block in the file).
//!
//! `provider` blocks are intentionally skipped — multiple `provider "aws"`
//! blocks with distinct `alias` values are valid and expected.

use std::collections::HashMap;

use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

pub fn duplicate_definition_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    // Address → range of its FIRST occurrence. Second and later
    // occurrences are flagged, pointing back at the first.
    let mut seen: HashMap<String, Range> = HashMap::new();

    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        match block.ident.as_str() {
            "variable" | "output" | "module" => {
                let Some((name, range)) = single_label(block, rope) else {
                    continue;
                };
                let key = format!("{}:{name}", block.ident.as_str());
                record(&mut seen, &mut out, key, range, block.ident.as_str(), &name);
            }
            "resource" | "data" => {
                let Some((ty, name, range)) = two_labels(block, rope) else {
                    continue;
                };
                let key = format!("{}:{ty}.{name}", block.ident.as_str());
                let kind = block.ident.as_str();
                record(&mut seen, &mut out, key, range, kind, &format!("{ty}.{name}"));
            }
            "locals" => {
                for entry in block.body.iter() {
                    let Some(attr) = entry.as_attribute() else {
                        continue;
                    };
                    let name = attr.key.as_str().to_string();
                    let Some(span) = attr.key.span() else { continue };
                    let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
                        continue;
                    };
                    record(&mut seen, &mut out, format!("local:{name}"), range, "local", &name);
                }
            }
            _ => {}
        }
    }

    out
}

/// Insert the address if new; otherwise emit a duplicate diagnostic on
/// `range`, citing the first occurrence's line.
fn record(
    seen: &mut HashMap<String, Range>,
    out: &mut Vec<Diagnostic>,
    key: String,
    range: Range,
    kind: &str,
    label: &str,
) {
    if let Some(first) = seen.get(&key) {
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("terraform-ls-rs".to_string()),
            message: format!(
                "duplicate {kind} `{label}` — already defined on line {}",
                first.start.line + 1
            ),
            ..Default::default()
        });
    } else {
        seen.insert(key, range);
    }
}

/// `(name, range)` for a single-label block (`variable "x"`), ranging on
/// the name label.
fn single_label(block: &Block, rope: &Rope) -> Option<(String, Range)> {
    let label = block.labels.first()?;
    let name = label_str(label)?.to_string();
    let range = hcl_span_to_lsp_range(rope, label.span()?).ok()?;
    Some((name, range))
}

/// `(type, name, range)` for a two-label block (`resource "T" "N"`),
/// ranging on the name label (falling back to the type label).
fn two_labels(block: &Block, rope: &Rope) -> Option<(String, String, Range)> {
    let ty = label_str(block.labels.first()?)?.to_string();
    let name_label = block.labels.get(1)?;
    let name = label_str(name_label)?.to_string();
    let span = name_label.span().or_else(|| block.labels.first().and_then(|l| l.span()))?;
    let range = hcl_span_to_lsp_range(rope, span).ok()?;
    Some((ty, name, range))
}

fn label_str(label: &BlockLabel) -> Option<&str> {
    match label {
        BlockLabel::String(s) => Some(s.value().as_str()),
        BlockLabel::Ident(i) => Some(i.as_str()),
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
        duplicate_definition_diagnostics(&body, &rope)
    }

    #[test]
    fn flags_duplicate_variable() {
        let d = diags("variable \"x\" {}\nvariable \"x\" {}\n");
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(d[0].message.contains("duplicate variable `x`"), "got: {}", d[0].message);
        // Flags the SECOND occurrence (line 1, 0-based).
        assert_eq!(d[0].range.start.line, 1);
        assert!(d[0].message.contains("line 1"), "should cite first occurrence: {}", d[0].message);
    }

    #[test]
    fn flags_duplicate_resource_same_type_and_name() {
        let d = diags(
            "resource \"aws_instance\" \"web\" {}\nresource \"aws_instance\" \"web\" {}\n",
        );
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("aws_instance.web"), "got: {}", d[0].message);
    }

    #[test]
    fn allows_same_name_different_type() {
        // `resource "aws_instance" "web"` and `data "aws_instance" "web"`
        // are distinct addresses.
        let d = diags(
            "resource \"aws_instance\" \"web\" {}\ndata \"aws_instance\" \"web\" {}\n",
        );
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn allows_same_name_across_kinds() {
        // A variable and an output may share a name.
        let d = diags("variable \"x\" {}\noutput \"x\" { value = 1 }\n");
        assert!(d.is_empty(), "got: {d:?}");
    }

    #[test]
    fn flags_duplicate_local_across_blocks() {
        let d = diags("locals { a = 1 }\nlocals { a = 2 }\n");
        assert_eq!(d.len(), 1, "got: {d:?}");
        assert!(d[0].message.contains("duplicate local `a`"), "got: {}", d[0].message);
    }

    #[test]
    fn flags_three_duplicates_as_two_diagnostics() {
        let d = diags("output \"o\" { value = 1 }\noutput \"o\" { value = 2 }\noutput \"o\" { value = 3 }\n");
        assert_eq!(d.len(), 2, "first is the original, 2nd and 3rd flagged: {d:?}");
    }

    #[test]
    fn ignores_provider_aliases() {
        let d = diags(concat!(
            "provider \"aws\" {\n  region = \"us-east-1\"\n}\n",
            "provider \"aws\" {\n  alias  = \"west\"\n  region = \"us-west-2\"\n}\n",
        ));
        assert!(d.is_empty(), "provider aliases are valid: {d:?}");
    }

    #[test]
    fn silent_for_unique_definitions() {
        let d = diags("variable \"a\" {}\nvariable \"b\" {}\nresource \"x\" \"y\" {}\n");
        assert!(d.is_empty(), "got: {d:?}");
    }
}
