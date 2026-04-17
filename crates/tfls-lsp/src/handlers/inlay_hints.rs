//! `textDocument/inlayHint` — show literal default values after
//! `var.<name>` references when the variable block declares a
//! literal-scalar `default`.
//!
//! The hint is computed on demand; we limit scanning to references
//! whose range falls inside the requested visible range.

use hcl_edit::expr::Expression;
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, InlayHintParams, Range};
use std::collections::HashMap;
use tfls_parser::ReferenceKind;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub async fn inlay_hint(
    backend: &Backend,
    params: InlayHintParams,
) -> jsonrpc::Result<Option<Vec<InlayHint>>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let Some(body) = doc.parsed.body.as_ref() else {
        return Ok(None);
    };

    // Build a map of variable name → literal default string.
    let defaults = collect_variable_defaults(body);
    if defaults.is_empty() {
        return Ok(None);
    }

    let mut hints = Vec::new();
    for reference in &doc.references {
        if let ReferenceKind::Variable { name } = &reference.kind {
            let range = reference.location.range();
            if !within(&params.range, range) {
                continue;
            }
            if let Some(def) = defaults.get(name) {
                hints.push(InlayHint {
                    position: range.end,
                    label: InlayHintLabel::String(format!(" = {def}")),
                    kind: Some(InlayHintKind::PARAMETER),
                    tooltip: None,
                    text_edits: None,
                    padding_left: Some(true),
                    padding_right: None,
                    data: None,
                });
            }
        }
    }

    if hints.is_empty() { Ok(None) } else { Ok(Some(hints)) }
}

fn collect_variable_defaults(body: &Body) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "variable" {
            continue;
        }
        let Some(name) = first_label(block) else {
            continue;
        };
        for s in block.body.iter() {
            if let Some(attr) = s.as_attribute() {
                if attr.key.as_str() == "default" {
                    if let Some(lit) = literal_scalar(&attr.value) {
                        out.insert(name.to_string(), lit);
                    }
                    break;
                }
            }
        }
    }
    out
}

fn first_label(block: &Block) -> Option<&str> {
    block.labels.first().map(|l| match l {
        BlockLabel::String(s) => s.value().as_str(),
        BlockLabel::Ident(i) => i.as_str(),
    })
}

/// Return the source-level representation of a literal scalar
/// expression (string, number, bool). Compound expressions are
/// skipped so hints don't get noisy.
fn literal_scalar(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(format!("\"{}\"", s.value())),
        Expression::Number(n) => Some(n.value().to_string()),
        Expression::Bool(b) => Some(b.value().to_string()),
        Expression::Null(_) => Some("null".to_string()),
        _ => None,
    }
}

fn within(outer: &Range, inner: Range) -> bool {
    (inner.start.line, inner.start.character)
        >= (outer.start.line, outer.start.character)
        && (inner.end.line, inner.end.character) <= (outer.end.line, outer.end.character)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    #[test]
    fn collects_string_default() {
        let body = parse_source(r#"variable "region" { default = "us-east-1" }"#)
            .body
            .expect("parses");
        let defs = collect_variable_defaults(&body);
        assert_eq!(defs.get("region"), Some(&"\"us-east-1\"".to_string()));
    }

    #[test]
    fn collects_numeric_default() {
        let body = parse_source(r#"variable "count" { default = 3 }"#)
            .body
            .expect("parses");
        let defs = collect_variable_defaults(&body);
        assert_eq!(defs.get("count"), Some(&"3".to_string()));
    }

    #[test]
    fn skips_non_literal_default() {
        let body = parse_source(r#"variable "x" { default = [1, 2, 3] }"#)
            .body
            .expect("parses");
        let defs = collect_variable_defaults(&body);
        assert!(!defs.contains_key("x"));
    }

    #[test]
    fn skips_variables_without_default() {
        let body = parse_source(r#"variable "x" {}"#).body.expect("parses");
        let defs = collect_variable_defaults(&body);
        assert!(defs.is_empty());
    }
}
