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
        } else if is_missing_variable_type(diag) {
            if let Some(action) = make_insert_variable_type_action(
                &uri,
                diag,
                body,
                &doc.rope,
                &doc.symbols,
                &backend.state,
            ) {
                actions.push(CodeActionOrCommand::CodeAction(action));
            }
        }
    }

    // "Fix all" — single action that adds `type = …` to every
    // untyped variable in the file with inferable type. Surfaced
    // independently of `params.context.diagnostics` so the user
    // can invoke it from a generic source-action menu without
    // having to position the cursor on a specific diagnostic.
    if let Some(action) =
        make_fix_all_variable_types_action(&uri, body, &doc.rope, &doc.symbols, &backend.state)
    {
        actions.push(CodeActionOrCommand::CodeAction(action));
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
    let (insert_pos, prefix) = insertion_position(block, rope)?;
    let indent = "  "; // two-space indent matching our formatter

    let new_text = format!("{prefix}{indent}{attr_name} = {placeholder}\n");
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

/// Insert new attributes at the top of the block body. Returns the
/// position to insert at + the prefix to prepend before the
/// caller's `key = value\n` line. When the block body already has
/// content (`{\n  …\n}`), we insert right after the opening
/// `{`'s newline and prepend nothing. When the body is empty
/// (`{}` or `{ }`), hcl-edit reports no body span; we drop the
/// insert immediately after the `{` and prepend a leading `\n` so
/// the closing brace ends up on its own line.
fn insertion_position(block: &Block, rope: &Rope) -> Option<(Position, &'static str)> {
    if let Some(body_span) = block.body.span() {
        // Non-empty body — body_span.start is the byte right after
        // `{`. Advance past the immediate newline so the inserted
        // line is placed below the brace.
        let text = rope
            .slice(rope.byte_to_char(body_span.start)..rope.len_chars())
            .to_string();
        let offset = text.find('\n').map_or(0, |i| i + 1);
        let insert_byte = body_span.start + offset;
        let pos = tfls_parser::byte_offset_to_lsp_position(rope, insert_byte).ok()?;
        return Some((pos, ""));
    }

    // Empty body. Locate the `{` from the block's overall span.
    let block_span = block.span()?;
    let block_text = rope
        .slice(rope.byte_to_char(block_span.start)..rope.byte_to_char(block_span.end))
        .to_string();
    let brace_off = block_text.find('{')?;
    let insert_byte = block_span.start + brace_off + 1;
    let pos = tfls_parser::byte_offset_to_lsp_position(rope, insert_byte).ok()?;
    Some((pos, "\n"))
}

/// Match the `terraform_typed_variables` warning so we can offer a
/// quick-fix that inserts the inferred `type = …` attribute.
fn is_missing_variable_type(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::WARNING)
        && diag.source.as_deref() == Some("terraform-ls-rs")
        && diag.message.contains("variable has no type")
}

fn make_insert_variable_type_action(
    uri: &Url,
    diag: &Diagnostic,
    body: &Body,
    rope: &Rope,
    symbols: &tfls_core::SymbolTable,
    state: &tfls_state::StateStore,
) -> Option<CodeAction> {
    let var_name = missing_attr_name(&diag.message)?.to_string();
    let Some(block) = find_variable_block(body, &var_name) else {
        tracing::info!(
            uri = %uri,
            var = %var_name,
            "code-action infer-type: variable block not found",
        );
        return None;
    };

    // Bail out if the block already has a `type` attribute — covers
    // the stale-diagnostic case where the user fixed the warning by
    // hand but the client still has it cached.
    if block_has_attribute(block, "type") {
        tracing::info!(
            uri = %uri,
            var = %var_name,
            "code-action infer-type: block already has type, skipping",
        );
        return None;
    }

    // Three sources, in priority order:
    //   1. The variable's own `default = …` literal.
    //   2. Values assigned via `*.tfvars` files in the same directory.
    //   3. Attributes on `module "X" { var_name = expr }` callers.
    //
    // (2) and (3) merge into the same per-dir map (`state.assigned_variable_types`),
    // and `merged_assigned_type` returns `Some(ty)` only when every
    // observed assignment yields the same shape — disagreement means
    // we don't know the canonical type, so we skip rather than guess.
    let inferred_from_default = symbols
        .variable_defaults
        .get(&var_name)
        .filter(|t| is_actionable_inference(t))
        .cloned();
    let module_dir_dbg = crate::handlers::util::parent_dir(uri);
    let merged_dbg = module_dir_dbg
        .as_deref()
        .and_then(|d| state.merged_assigned_type(d, &var_name));
    tracing::info!(
        uri = %uri,
        var = %var_name,
        module_dir = ?module_dir_dbg,
        from_default = ?inferred_from_default,
        from_merged = ?merged_dbg,
        "code-action infer-type: lookup",
    );
    let inferred = inferred_from_default.or_else(|| {
        let module_dir = crate::handlers::util::parent_dir(uri)?;
        let merged = state.merged_assigned_type(&module_dir, &var_name)?;
        if !is_actionable_inference(&merged) {
            return None;
        }
        Some(merged)
    })?;
    let rendered = inferred.to_string();
    let (insert_pos, prefix) = insertion_position(block, rope)?;
    let indent = "  ";
    let new_text = format!("{prefix}{indent}type = {rendered}\n");

    let edit = TextEdit {
        range: Range {
            start: insert_pos,
            end: insert_pos,
        },
        new_text,
    };
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    let title_source = if symbols
        .variable_defaults
        .get(&var_name)
        .is_some_and(is_actionable_inference)
    {
        "default"
    } else {
        "tfvars / module callers"
    };

    Some(CodeAction {
        title: format!("Set variable type to `{rendered}` from {title_source}"),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diag.clone()]),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        // Object/tuple shapes can be coarse — leave the action
        // available but not preferred so other plugins can win.
        is_preferred: Some(matches!(
            inferred,
            tfls_core::variable_type::VariableType::Primitive(_)
        )),
        ..Default::default()
    })
}

/// Walk every `variable` block in the file. For each one missing a
/// `type` attribute that has a concrete inferable shape (default
/// literal, tfvars assignment, or module-caller value), produce a
/// single combined `CodeAction` whose `WorkspaceEdit` inserts
/// `type = …` into all of them at once.
///
/// Returns `None` when the file has no fixable variables — the
/// handler then skips this action so the menu doesn't show a
/// no-op entry.
fn make_fix_all_variable_types_action(
    uri: &Url,
    body: &Body,
    rope: &Rope,
    symbols: &tfls_core::SymbolTable,
    state: &tfls_state::StateStore,
) -> Option<CodeAction> {
    let module_dir = crate::handlers::util::parent_dir(uri);
    let mut edits: Vec<TextEdit> = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else { continue };
        if block.ident.as_str() != "variable" {
            continue;
        }
        if block_has_attribute(block, "type") {
            continue;
        }
        let Some(name) = block.labels.first().and_then(label_str) else {
            continue;
        };
        // Same priority order as the per-diagnostic action:
        // 1. variable's own `default = …`
        // 2. merged tfvars / module-caller assignments
        let inferred_from_default = symbols
            .variable_defaults
            .get(name)
            .filter(|t| is_actionable_inference(t))
            .cloned();
        let inferred = inferred_from_default.or_else(|| {
            let dir = module_dir.as_deref()?;
            let merged = state.merged_assigned_type(dir, name)?;
            if !is_actionable_inference(&merged) {
                return None;
            }
            Some(merged)
        });
        let Some(ty) = inferred else { continue };

        let Some((insert_pos, prefix)) = insertion_position(block, rope) else {
            continue;
        };
        let new_text = format!("{prefix}  type = {ty}\n");
        edits.push(TextEdit {
            range: Range {
                start: insert_pos,
                end: insert_pos,
            },
            new_text,
        });
    }
    if edits.is_empty() {
        return None;
    }
    let count = edits.len();
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri.clone(), edits);
    Some(CodeAction {
        title: format!(
            "Set variable types: infer `type = …` for {count} untyped variable{plural}",
            plural = if count == 1 { "" } else { "s" },
        ),
        // `source.fixAll` lets clients trigger this from a
        // generic "fix all" / "source action" menu without
        // requiring a specific diagnostic at the cursor.
        kind: Some(CodeActionKind::SOURCE_FIX_ALL),
        diagnostics: None,
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        is_preferred: None,
        ..Default::default()
    })
}

fn find_variable_block<'b>(body: &'b Body, name: &str) -> Option<&'b Block> {
    for structure in body.iter() {
        let block = structure.as_block()?;
        if block.ident.as_str() != "variable" {
            continue;
        }
        let label = block.labels.first().and_then(label_str)?;
        if label == name {
            return Some(block);
        }
    }
    None
}

fn block_has_attribute(block: &Block, name: &str) -> bool {
    block.body.iter().any(|s| {
        s.as_attribute()
            .is_some_and(|a| a.key.as_str() == name)
    })
}

/// Decide whether a `VariableType` is concrete enough to
/// confidently splice into the source.
///
/// Skip:
/// - `Any` — already filtered out by the symbol-table builder
///   (`tfls-parser/src/traversal.rs`), but defensive.
/// - Empty `Tuple([])` — `default = []`. Could be list/set of any
///   primitive; a wrong guess wastes the user's time.
/// - Empty `Object({})` — `default = {}`. Same problem.
fn is_actionable_inference(ty: &tfls_core::variable_type::VariableType) -> bool {
    use tfls_core::variable_type::VariableType;
    match ty {
        VariableType::Any => false,
        VariableType::Tuple(items) if items.is_empty() => false,
        VariableType::Object(fields) if fields.is_empty() => false,
        _ => true,
    }
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
