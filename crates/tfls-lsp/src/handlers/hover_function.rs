//! Hover for interpolation function calls.
//!
//! Two cursor positions produce a function hover:
//! 1. **On the function name itself** — e.g. `tostr|ing(x)`. We identify the
//!    ident at the cursor and check whether a `(` follows (allowing for
//!    optional whitespace).
//! 2. **Inside the call's argument list** — e.g. `length(var.names|)`. We
//!    reuse [`crate::handlers::signature_help::enclosing_call`] which
//!    already handles nested calls and string-escaped commas.
//!
//! Function signatures are loaded at startup (bundled or CLI-fetched) into
//! `state.functions` — this handler only queries that index.

use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use std::sync::Arc;
use tfls_parser::lsp_position_to_byte_offset;
use tfls_schema::FunctionSignature;
use tfls_state::{DocumentState, StateStore};

use crate::handlers::signature_help::{
    enclosing_call, identifier_at, qualified_name_ending_at, resolve_function, type_label,
};

pub fn function_hover(state: &StateStore, doc: &DocumentState, pos: Position) -> Option<Hover> {
    let offset = lsp_position_to_byte_offset(&doc.rope, pos).ok()?;
    let text = doc.rope.to_string();

    if let Some(sig) = lookup_at_cursor(state, &text, offset) {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: render(&sig.0, &sig.1),
            }),
            range: None,
        });
    }
    None
}

/// Look up the function the cursor is on or inside. Returns `(name, sig)`.
fn lookup_at_cursor(
    state: &StateStore,
    text: &str,
    offset: usize,
) -> Option<(String, Arc<FunctionSignature>)> {
    // Case 1: cursor is on an identifier followed (possibly after whitespace)
    // by `(`. Treat the identifier as a function name. Walk back over
    // `provider::<local>::` if present so qualified provider-defined
    // function calls resolve.
    if let Some((_, range)) = identifier_at(text, offset) {
        if followed_by_open_paren(text, range.end) {
            if let Some(name) = qualified_name_ending_at(text, range.end) {
                if let Some((resolved, sig)) = resolve_function(state, &name) {
                    return Some((resolved, sig));
                }
            }
        }
    }

    // Case 2: cursor is inside the argument list of an unclosed call.
    let (name, _arg_idx) = enclosing_call(text, offset)?;
    let (resolved, sig) = resolve_function(state, &name)?;
    Some((resolved, sig))
}

/// True if `text[from..]` starts with a `(`, skipping ASCII whitespace.
/// Terraform parsers don't actually allow spaces between a function name
/// and its `(`, but being permissive here costs nothing and makes the
/// hover robust to editor state mid-typing.
fn followed_by_open_paren(text: &str, from: usize) -> bool {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b' ' || b == b'\t' {
            i += 1;
            continue;
        }
        return b == b'(';
    }
    false
}

fn render(name: &str, sig: &FunctionSignature) -> String {
    let mut out = String::new();
    out.push_str(&format!("**function** `{name}`\n\n"));

    if let Some(desc) = sig.description.as_deref() {
        if !desc.trim().is_empty() {
            out.push_str(desc);
            out.push_str("\n\n");
        }
    }

    out.push_str("```hcl\n");
    out.push_str(&sig.label(name));
    out.push_str("\n```\n");

    if !sig.parameters.is_empty() || sig.variadic_parameter.is_some() {
        out.push_str("\n**Parameters**\n");
        for p in &sig.parameters {
            append_param_line(&mut out, &p.name, &p.r#type, p.description.as_deref(), false);
        }
        if let Some(v) = &sig.variadic_parameter {
            append_param_line(&mut out, &v.name, &v.r#type, v.description.as_deref(), true);
        }
    }

    out
}

fn append_param_line(
    out: &mut String,
    name: &str,
    ty: &sonic_rs::Value,
    description: Option<&str>,
    variadic: bool,
) {
    let suffix = if variadic { "..." } else { "" };
    out.push_str(&format!("- `{name}{suffix}`: {}", type_label(ty)));
    if let Some(desc) = description {
        let desc = desc.trim();
        if !desc.is_empty() {
            out.push_str(" — ");
            out.push_str(desc);
        }
    }
    out.push('\n');
}
