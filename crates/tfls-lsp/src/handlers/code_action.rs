//! `textDocument/codeAction` — quick fixes derived from our own
//! diagnostics.
//!
//! Currently provides one fix: insert any required attributes that a
//! resource block is missing.

use std::collections::HashMap;

use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, BlockLabel, Body};
use sonic_rs::JsonValueTrait;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
    Diagnostic, DiagnosticSeverity, Position, Range, TextEdit, Url, WorkspaceEdit,
};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub async fn code_action(
    backend: &Backend,
    params: CodeActionParams,
) -> jsonrpc::Result<Option<CodeActionResponse>> {
    let uri = params.text_document.uri.clone();
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let Some(body) = doc.parsed.body.as_ref() else {
        return Ok(None);
    };

    let mut actions: Vec<CodeActionOrCommand> = Vec::new();

    for diag in &params.context.diagnostics {
        if is_missing_required(diag) {
            if let Some(action) =
                make_insert_required_action(backend, &uri, diag, body, &doc.rope)
            {
                actions.push(CodeActionOrCommand::CodeAction(action));
            }
        }
    }

    if actions.is_empty() { Ok(None) } else { Ok(Some(actions)) }
}

fn is_missing_required(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::ERROR)
        && diag.source.as_deref() == Some("terraform-ls-rs")
        && diag.message.contains("missing required attribute")
}

/// Pull the attribute name out of a message like
/// `missing required attribute \`ami\``.
fn missing_attr_name(message: &str) -> Option<&str> {
    let start = message.find('`')?;
    let rest = &message[start + 1..];
    let end = rest.find('`')?;
    Some(&rest[..end])
}

fn make_insert_required_action(
    backend: &Backend,
    uri: &Url,
    diag: &Diagnostic,
    body: &Body,
    rope: &Rope,
) -> Option<CodeAction> {
    let attr_name = missing_attr_name(&diag.message)?.to_string();
    let (block, _block_range) = find_block_at(body, rope, diag.range.start)?;
    let (block_type, _) = resource_header(block)?;
    let schema = backend.state.resource_schema(&block_type)?;
    let attr_schema = schema.block.attributes.get(&attr_name)?;

    let placeholder = placeholder_for(attr_schema);
    let insert_pos = insertion_position(block, rope)?;
    let indent = "  "; // two-space indent matching our formatter

    let new_text = format!("{indent}{attr_name} = {placeholder}\n");
    let edit = TextEdit {
        range: Range {
            start: insert_pos,
            end: insert_pos,
        },
        new_text,
    };

    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: format!("Insert missing required attribute `{attr_name}`"),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diag.clone()]),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        is_preferred: Some(true),
        ..Default::default()
    })
}

/// Find the innermost resource/data block whose span contains `pos`.
fn find_block_at<'b>(
    body: &'b Body,
    rope: &Rope,
    pos: Position,
) -> Option<(&'b Block, Range)> {
    for structure in body.iter() {
        let block = structure.as_block()?;
        let span = block.span()?;
        let range = hcl_span_to_lsp_range(rope, span).ok()?;
        if !contains(&range, pos) {
            continue;
        }
        if matches!(block.ident.as_str(), "resource" | "data") {
            return Some((block, range));
        }
    }
    None
}

fn contains(range: &Range, pos: Position) -> bool {
    let after_start = (pos.line, pos.character) >= (range.start.line, range.start.character);
    let before_end = (pos.line, pos.character) <= (range.end.line, range.end.character);
    after_start && before_end
}

fn resource_header(block: &Block) -> Option<(String, String)> {
    let labels = &block.labels;
    let ty = label_str(labels.first()?)?.to_string();
    let name = label_str(labels.get(1)?)?.to_string();
    Some((ty, name))
}

fn label_str(label: &BlockLabel) -> Option<&str> {
    match label {
        BlockLabel::String(s) => Some(s.value().as_str()),
        BlockLabel::Ident(i) => Some(i.as_str()),
    }
}

/// Insert new attributes at the top of the block body — just after the
/// opening `{`. We find the offset of the `{`, advance past it, past
/// its trailing newline if present, and return that as an LSP
/// position.
fn insertion_position(block: &Block, rope: &Rope) -> Option<Position> {
    let body_span = block.body.span()?;
    // body_span.start is the byte offset immediately after `{`.
    // Advance past a following newline so the inserted line lives on
    // its own row.
    let text = rope.slice(rope.byte_to_char(body_span.start)..rope.len_chars()).to_string();
    let offset_in_body = text.find('\n').map_or(0, |i| i + 1);
    let insert_byte = body_span.start + offset_in_body;

    tfls_parser::byte_offset_to_lsp_position(rope, insert_byte).ok()
}

fn placeholder_for(attr: &tfls_schema::AttributeSchema) -> &'static str {
    // Quick heuristic based on the primitive type name.
    if let Some(ty) = attr.r#type.as_str() {
        match ty {
            "string" => "\"\"",
            "number" => "0",
            "bool" => "false",
            _ => "null",
        }
    } else {
        "null"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn extracts_attribute_name_from_message() {
        assert_eq!(
            missing_attr_name("missing required attribute `ami`"),
            Some("ami")
        );
        assert_eq!(missing_attr_name("no ticks here"), None);
    }
}
