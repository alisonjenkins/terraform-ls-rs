//! Types for `tofu metadata functions -json` / `terraform metadata
//! functions -json`.
//!
//! Deserialised with `sonic-rs`. Bundled as a gzipped snapshot at
//! `schemas/functions.opentofu.json.gz` for offline use when no CLI
//! binary is available.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Top-level document: `{ format_version, function_signatures }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionsSchema {
    pub format_version: String,
    #[serde(default)]
    pub function_signatures: HashMap<String, FunctionSignature>,
}

/// Signature of a single built-in function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionSignature {
    #[serde(default)]
    pub description: Option<String>,
    /// Return type — either a primitive string or a compound array.
    #[serde(default)]
    pub return_type: sonic_rs::Value,
    #[serde(default)]
    pub parameters: Vec<FunctionParameter>,
    #[serde(default)]
    pub variadic_parameter: Option<FunctionParameter>,
}

/// A single positional or variadic parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionParameter {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub r#type: sonic_rs::Value,
    #[serde(default)]
    pub is_nullable: bool,
}

impl FunctionSignature {
    /// Render a full signature as displayed in signatureHelp
    /// (e.g. `format(format: string, args: dynamic...)`).
    pub fn label(&self, name: &str) -> String {
        let mut parts: Vec<String> = self
            .parameters
            .iter()
            .map(|p| format!("{}: {}", p.name, type_label(&p.r#type)))
            .collect();
        if let Some(v) = &self.variadic_parameter {
            parts.push(format!("{}: {}...", v.name, type_label(&v.r#type)));
        }
        format!("{name}({})", parts.join(", "))
    }

    /// A parameter by index — variadic parameter repeats.
    pub fn parameter_for_index(&self, index: usize) -> Option<&FunctionParameter> {
        if index < self.parameters.len() {
            return self.parameters.get(index);
        }
        self.variadic_parameter.as_ref()
    }
}

fn type_label(ty: &sonic_rs::Value) -> String {
    use sonic_rs::JsonValueTrait;
    if let Some(s) = ty.as_str() {
        return s.to_string();
    }
    // Compound types render as `list(string)` etc.
    if ty.is_array() {
        // Use the raw JSON text as an approximation.
        return ty.to_string();
    }
    "dynamic".to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_simple_function() {
        let json = r#"{
            "format_version": "1.0",
            "function_signatures": {
                "abs": {
                    "description": "absolute value",
                    "return_type": "number",
                    "parameters": [{"name": "num", "type": "number"}]
                }
            }
        }"#;
        let schema: FunctionsSchema = sonic_rs::from_str(json).expect("parse");
        let abs = schema.function_signatures.get("abs").expect("abs");
        assert_eq!(abs.parameters.len(), 1);
        assert_eq!(abs.label("abs"), "abs(num: number)");
    }

    #[test]
    fn deserialises_variadic_function() {
        let json = r#"{
            "format_version": "1.0",
            "function_signatures": {
                "format": {
                    "return_type": "string",
                    "parameters": [{"name": "format", "type": "string"}],
                    "variadic_parameter": {"name": "args", "type": "dynamic"}
                }
            }
        }"#;
        let schema: FunctionsSchema = sonic_rs::from_str(json).expect("parse");
        let f = schema.function_signatures.get("format").expect("format");
        assert_eq!(f.label("format"), "format(format: string, args: dynamic...)");
        assert_eq!(
            f.parameter_for_index(0).expect("p0").name,
            "format"
        );
        // Index 1 falls through to variadic.
        assert_eq!(
            f.parameter_for_index(1).expect("variadic").name,
            "args"
        );
        assert_eq!(
            f.parameter_for_index(99).expect("variadic again").name,
            "args"
        );
    }

    #[test]
    fn deserialises_real_bundled_snapshot() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let bytes = include_bytes!("../../../schemas/functions.opentofu.json.gz");
        let mut decoder = GzDecoder::new(&bytes[..]);
        let mut json = String::new();
        decoder.read_to_string(&mut json).expect("decompress");

        let schema: FunctionsSchema = sonic_rs::from_str(&json).expect("parse");
        assert!(
            schema.function_signatures.len() > 50,
            "expected many functions, got {}",
            schema.function_signatures.len()
        );
        assert!(schema.function_signatures.contains_key("abs"));
        assert!(schema.function_signatures.contains_key("format"));
        assert!(schema.function_signatures.contains_key("jsonencode"));
    }
}
