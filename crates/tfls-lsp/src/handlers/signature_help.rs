//! `textDocument/signatureHelp` — parameter help for built-in
//! Terraform / OpenTofu functions.
//!
//! The heavy lifting (fetching + caching signatures) is done at
//! workspace-init time by `Job::FetchFunctions`. This handler just
//! locates the enclosing function call in the source text around the
//! cursor and looks up the signature.

use lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Position,
    SignatureHelp, SignatureHelpParams, SignatureInformation,
};
use tfls_parser::lsp_position_to_byte_offset;
use tfls_schema::FunctionSignature;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub async fn signature_help(
    backend: &Backend,
    params: SignatureHelpParams,
) -> jsonrpc::Result<Option<SignatureHelp>> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let offset = match lsp_position_to_byte_offset(&doc.rope, pos) {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    let text = doc.rope.to_string();

    let Some((name, arg_index)) = enclosing_call(&text, offset) else {
        return Ok(None);
    };

    let Some(sig) = backend.state.functions.get(&name).map(|s| s.clone()) else {
        return Ok(None);
    };

    Ok(Some(render_signature(&name, &sig, arg_index)))
}

/// Inspect `text[..offset]` to find the innermost `ident(` whose
/// matching `)` has not yet been typed, and the current argument
/// index (count of top-level commas between `(` and the cursor).
/// Returns `None` if the cursor is not inside any call.
pub fn enclosing_call(text: &str, offset: usize) -> Option<(String, usize)> {
    if offset > text.len() {
        return None;
    }
    let before = text.get(..offset)?;

    // Walk right-to-left; track nesting of parens/brackets/braces.
    // When we meet an unmatched `(`, the identifier immediately to
    // its left is the function name. Commas encountered at depth 0
    // (relative to that starting point) are argument separators.
    let bytes = before.as_bytes();
    let mut depth_paren: i32 = 0;
    let mut depth_bracket: i32 = 0;
    let mut depth_brace: i32 = 0;
    let mut commas: usize = 0;
    let mut in_string = false;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        let c = bytes[i];
        if in_string {
            if c == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b')' => depth_paren += 1,
            b']' => depth_bracket += 1,
            b'}' => depth_brace += 1,
            b'(' => {
                if depth_paren == 0 && depth_bracket == 0 && depth_brace == 0 {
                    let name = identifier_ending_at(before, i)?;
                    return Some((name, commas));
                }
                depth_paren -= 1;
            }
            b'[' => depth_bracket -= 1,
            b'{' => depth_brace -= 1,
            b',' if depth_paren == 0 && depth_bracket == 0 && depth_brace == 0 => {
                commas += 1;
            }
            _ => {}
        }
    }
    None
}

/// Scan backward from `end` while bytes are valid identifier chars
/// (`A-Za-z0-9_`). Returns the identifier, or `None` if it's empty.
fn identifier_ending_at(text: &str, end: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut start = end;
    while start > 0 {
        let b = bytes[start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == end {
        None
    } else {
        text.get(start..end).map(str::to_string)
    }
}

fn render_signature(
    name: &str,
    sig: &FunctionSignature,
    active_parameter: usize,
) -> SignatureHelp {
    let label = sig.label(name);

    // Build parameter labels with character offsets so editors can
    // underline the active parameter in the signature label.
    let mut parameters: Vec<ParameterInformation> = Vec::new();
    let prefix = format!("{name}(");
    let mut cursor = prefix.len() as u32;

    let mut params_with_trailing: Vec<(String, Option<String>)> = sig
        .parameters
        .iter()
        .map(|p| {
            (
                format!("{}: {}", p.name, type_label(&p.r#type)),
                p.description.clone(),
            )
        })
        .collect();
    if let Some(v) = &sig.variadic_parameter {
        params_with_trailing.push((
            format!("{}: {}...", v.name, type_label(&v.r#type)),
            v.description.clone(),
        ));
    }

    let total = params_with_trailing.len();
    for (idx, (piece, desc)) in params_with_trailing.into_iter().enumerate() {
        let piece_len = piece.len() as u32;
        parameters.push(ParameterInformation {
            label: ParameterLabel::LabelOffsets([cursor, cursor + piece_len]),
            documentation: desc.map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d,
                })
            }),
        });
        cursor += piece_len;
        if idx + 1 != total {
            cursor += 2; // ", "
        }
    }

    let sig_info = SignatureInformation {
        label,
        documentation: sig.description.clone().map(|d| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: d,
            })
        }),
        parameters: Some(parameters),
        active_parameter: Some(clamp_active(active_parameter, sig) as u32),
    };

    SignatureHelp {
        signatures: vec![sig_info],
        active_signature: Some(0),
        active_parameter: Some(clamp_active(active_parameter, sig) as u32),
    }
}

fn clamp_active(idx: usize, sig: &FunctionSignature) -> usize {
    let fixed = sig.parameters.len();
    if sig.variadic_parameter.is_some() {
        // Variadic occupies index = fixed, so clamp so extra args highlight it.
        idx.min(fixed)
    } else if fixed == 0 {
        0
    } else {
        idx.min(fixed - 1)
    }
}

fn type_label(ty: &sonic_rs::Value) -> String {
    use sonic_rs::JsonValueTrait;
    if let Some(s) = ty.as_str() {
        return s.to_string();
    }
    if ty.is_array() {
        return ty.to_string();
    }
    "dynamic".to_string()
}

// `Position` is used through `params.text_document_position_params`;
// this blanket allow keeps the import clean for downstream readers.
#[allow(dead_code)]
fn _position_marker(_p: Position) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn enclosing_call_finds_outer_function_name() {
        let (name, idx) = enclosing_call("format(\"%s\", ", 13).expect("call");
        assert_eq!(name, "format");
        assert_eq!(idx, 1);
    }

    #[test]
    fn enclosing_call_handles_nested_calls() {
        let src = r#"format("%d", length("#;
        let (name, idx) = enclosing_call(src, src.len()).expect("inner call");
        // Cursor is inside `length(` which is the innermost call.
        assert_eq!(name, "length");
        assert_eq!(idx, 0);
    }

    #[test]
    fn enclosing_call_ignores_commas_inside_nested_parens() {
        let src = "format(abs(1, 2), ";
        let (name, idx) = enclosing_call(src, src.len()).expect("call");
        assert_eq!(name, "format");
        assert_eq!(idx, 1);
    }

    #[test]
    fn enclosing_call_ignores_commas_inside_strings() {
        let src = r#"format("a, b, c", "#;
        let (name, idx) = enclosing_call(src, src.len()).expect("call");
        assert_eq!(name, "format");
        assert_eq!(idx, 1);
    }

    #[test]
    fn enclosing_call_returns_none_outside_any_call() {
        assert!(enclosing_call("variable \"x\" {}", 10).is_none());
    }

    #[test]
    fn enclosing_call_counts_multiple_args() {
        let src = "merge(a, b, c, ";
        let (name, idx) = enclosing_call(src, src.len()).expect("call");
        assert_eq!(name, "merge");
        assert_eq!(idx, 3);
    }

    #[test]
    fn enclosing_call_returns_none_when_closed() {
        let src = "format(\"%s\", x)";
        assert!(enclosing_call(src, src.len()).is_none());
    }

    #[test]
    fn identifier_ending_at_walks_back_through_alnum() {
        assert_eq!(
            identifier_ending_at("foo(bar_baz123", 14),
            Some("bar_baz123".to_string())
        );
    }

    #[test]
    fn identifier_ending_at_returns_none_for_empty_prefix() {
        assert!(identifier_ending_at("foo +(", 5).is_none());
    }

    #[test]
    fn clamp_active_limits_to_last_fixed_param() {
        let sig = FunctionSignature {
            description: None,
            return_type: sonic_rs::Value::default(),
            parameters: vec![
                tfls_schema::FunctionParameter {
                    name: "a".into(),
                    description: None,
                    r#type: sonic_rs::Value::default(),
                    is_nullable: false,
                },
                tfls_schema::FunctionParameter {
                    name: "b".into(),
                    description: None,
                    r#type: sonic_rs::Value::default(),
                    is_nullable: false,
                },
            ],
            variadic_parameter: None,
        };
        assert_eq!(clamp_active(5, &sig), 1);
    }
}
