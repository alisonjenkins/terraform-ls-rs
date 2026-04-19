//! `.tf.json` → HCL source conversion.
//!
//! Terraform files come in two equivalent surface syntaxes: native
//! HCL (`.tf`) and JSON (`.tf.json`). The two share the same
//! semantic model — JSON is a straight transliteration:
//!
//! - top-level object keys map to block idents (`resource`, `data`,
//!   `variable`, …) or root attribute names (`locals`)
//! - nested objects are block labels + bodies, nested one per layer
//! - strings, numbers, bools, arrays round-trip as HCL literals
//! - template strings stay as JSON strings but retain `${…}` syntax
//!
//! Rather than build a full JSON-aware `hcl_edit::Body` (which would
//! require synthesizing `Span` + `Decor` values pointing into the
//! JSON source), we convert JSON → HCL text and re-parse. Spans
//! end up pointing into the *generated* HCL, which is wrong for
//! exact LSP diagnostics, but it's enough for the common case of
//! "parse this `.tf.json` and evaluate downstream completion /
//! reference / validation logic on an equivalent structure". A
//! follow-up pass can pipe JSON spans through a dedicated
//! representation.

use crate::error::ParseError;
use crate::parse::{ParsedFile, parse_source};

/// Top-level object keys recognised by Terraform's JSON syntax.
/// Anything else is reported as `terraform_json_syntax` — or in our
/// wrapper, a parse error.
const TOP_LEVEL_KEYS: &[&str] = &[
    "terraform",
    "resource",
    "data",
    "variable",
    "output",
    "locals",
    "module",
    "provider",
    "moved",
    "check",
    "import",
    "removed",
];

/// Parse a `.tf.json` document. Returns the same `ParsedFile` shape
/// the HCL parser returns, so downstream code is agnostic to syntax.
///
/// On malformed JSON, returns an empty body with one parse error. On
/// valid JSON that doesn't match Terraform's JSON schema, returns a
/// parse error describing the offending position.
pub fn parse_json_source(source: &str) -> ParsedFile {
    let value: serde_json::Value = match serde_json::from_str(source) {
        Ok(v) => v,
        Err(e) => {
            return ParsedFile {
                body: None,
                errors: vec![ParseError::Json {
                    message: format!("invalid JSON: {e}"),
                }],
            };
        }
    };
    let root = match value.as_object() {
        Some(obj) => obj,
        None => {
            return ParsedFile {
                body: None,
                errors: vec![ParseError::Json {
                    message: "`.tf.json` must be a JSON object at the top level".to_string(),
                }],
            };
        }
    };
    let mut errors: Vec<ParseError> = Vec::new();
    let mut hcl = String::new();
    for (key, val) in root {
        if !TOP_LEVEL_KEYS.contains(&key.as_str()) {
            errors.push(ParseError::Json {
                message: format!("unknown top-level key `{key}` in `.tf.json`"),
            });
            continue;
        }
        if let Err(e) = write_top_level(&mut hcl, key, val) {
            errors.push(ParseError::Json { message: e });
        }
    }
    let parsed = parse_source(&hcl);
    let mut all_errors = errors;
    all_errors.extend(parsed.errors);
    ParsedFile {
        body: parsed.body,
        errors: all_errors,
    }
}

fn write_top_level(out: &mut String, key: &str, val: &serde_json::Value) -> Result<(), String> {
    match key {
        "terraform" => write_block(out, key, &[], val, 0),
        "locals" => write_locals(out, val),
        "resource" | "data" | "variable" | "output" | "module" | "provider" | "moved"
        | "check" | "import" | "removed" => write_labeled_group(out, key, val),
        _ => Err(format!("unsupported top-level key `{key}`")),
    }
}

/// `"resource": { "TYPE": { "NAME": { …body… } } }` flattens into
/// one `resource "TYPE" "NAME" { … }` block per leaf. `data` /
/// `variable` / `output` / etc. follow the same layered pattern.
fn write_labeled_group(out: &mut String, ident: &str, val: &serde_json::Value) -> Result<(), String> {
    let first_layer = val
        .as_object()
        .ok_or_else(|| format!("`{ident}` must be an object"))?;
    for (label, inner) in first_layer {
        // resource / data nest twice: type → name → body.
        // variable / output / module / provider nest once: name → body.
        let needs_two_labels = matches!(ident, "resource" | "data");
        if needs_two_labels {
            let second_layer = inner
                .as_object()
                .ok_or_else(|| format!("`{ident}.{label}` must be an object of named blocks"))?;
            for (name, body_val) in second_layer {
                write_block(out, ident, &[label, name], body_val, 0)?;
            }
        } else {
            write_block(out, ident, &[label], inner, 0)?;
        }
    }
    Ok(())
}

fn write_locals(out: &mut String, val: &serde_json::Value) -> Result<(), String> {
    let obj = val
        .as_object()
        .ok_or_else(|| "`locals` must be an object".to_string())?;
    out.push_str("locals {\n");
    for (name, v) in obj {
        write_attr(out, name, v, 1)?;
    }
    out.push_str("}\n");
    Ok(())
}

fn write_block(
    out: &mut String,
    ident: &str,
    labels: &[&str],
    body_val: &serde_json::Value,
    indent: usize,
) -> Result<(), String> {
    let body = body_val
        .as_object()
        .ok_or_else(|| format!("`{ident}` body must be an object"))?;
    indent_push(out, indent);
    out.push_str(ident);
    for label in labels {
        out.push_str(" \"");
        out.push_str(&escape_string(label));
        out.push('"');
    }
    out.push_str(" {\n");
    for (key, val) in body {
        write_body_entry(out, key, val, indent + 1)?;
    }
    indent_push(out, indent);
    out.push_str("}\n");
    Ok(())
}

fn write_body_entry(
    out: &mut String,
    key: &str,
    val: &serde_json::Value,
    indent: usize,
) -> Result<(), String> {
    // An object value whose first layer looks like `{ "label": {...} }`
    // is a nested block list in Terraform JSON. We can't perfectly
    // distinguish nested blocks from plain object attributes without
    // schema knowledge; treat values that are *arrays of objects* as
    // nested blocks (provider-schema shape) and plain objects as
    // attribute values.
    if let Some(arr) = val.as_array() {
        if arr.iter().all(|v| v.is_object()) && !arr.is_empty() {
            for entry in arr {
                write_block(out, key, &[], entry, indent)?;
            }
            return Ok(());
        }
    }
    write_attr(out, key, val, indent)
}

fn write_attr(
    out: &mut String,
    key: &str,
    val: &serde_json::Value,
    indent: usize,
) -> Result<(), String> {
    indent_push(out, indent);
    out.push_str(key);
    out.push_str(" = ");
    write_value(out, val, indent)?;
    out.push('\n');
    Ok(())
}

fn write_value(
    out: &mut String,
    val: &serde_json::Value,
    indent: usize,
) -> Result<(), String> {
    match val {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => {
            out.push('"');
            out.push_str(&escape_string(s));
            out.push('"');
        }
        serde_json::Value::Array(arr) => {
            out.push('[');
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, v, indent)?;
            }
            out.push(']');
        }
        serde_json::Value::Object(obj) => {
            out.push_str("{\n");
            for (k, v) in obj {
                indent_push(out, indent + 1);
                out.push_str(k);
                out.push_str(" = ");
                write_value(out, v, indent + 1)?;
                out.push('\n');
            }
            indent_push(out, indent);
            out.push('}');
        }
    }
    Ok(())
}

fn indent_push(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_json_object() {
        let parsed = parse_json_source("{}");
        assert!(parsed.body.is_some(), "got: {parsed:?}");
        assert!(!parsed.has_errors(), "got: {:?}", parsed.errors);
    }

    #[test]
    fn parses_variable_block_from_json() {
        let parsed = parse_json_source(
            r#"{
                "variable": {
                    "region": { "type": "string" }
                }
            }"#,
        );
        assert!(parsed.body.is_some(), "got errors: {:?}", parsed.errors);
        let body = parsed.body.unwrap();
        let has_variable = body
            .iter()
            .any(|s| s.as_block().is_some_and(|b| b.ident.as_str() == "variable"));
        assert!(has_variable, "body didn't round-trip to a variable block");
    }

    #[test]
    fn parses_resource_two_label_block() {
        let parsed = parse_json_source(
            r#"{
                "resource": {
                    "aws_instance": {
                        "web": { "ami": "ami-123" }
                    }
                }
            }"#,
        );
        assert!(parsed.body.is_some(), "got: {:?}", parsed.errors);
        let body = parsed.body.unwrap();
        let block = body
            .iter()
            .find_map(|s| s.as_block())
            .expect("has block");
        assert_eq!(block.ident.as_str(), "resource");
        assert_eq!(block.labels.len(), 2);
    }

    #[test]
    fn parses_locals() {
        let parsed = parse_json_source(
            r#"{
                "locals": { "x": 1, "y": "hello" }
            }"#,
        );
        assert!(parsed.body.is_some());
        assert!(!parsed.has_errors(), "got: {:?}", parsed.errors);
    }

    #[test]
    fn rejects_non_object_root() {
        let parsed = parse_json_source("[]");
        assert!(parsed.has_errors());
    }

    #[test]
    fn rejects_malformed_json() {
        let parsed = parse_json_source("{invalid}");
        assert!(parsed.has_errors());
    }

    #[test]
    fn flags_unknown_top_level_key() {
        let parsed = parse_json_source(r#"{ "unknown_root": {} }"#);
        assert!(parsed.has_errors());
        let msg = &parsed.errors[0];
        assert!(format!("{msg:?}").contains("unknown_root"));
    }
}
