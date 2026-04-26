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

    tracing::info!(
        uri = %uri,
        diag_count = params.context.diagnostics.len(),
        diags = ?params.context.diagnostics.iter().map(|d| (
            d.severity, d.source.as_deref().unwrap_or(""), d.message.clone()
        )).collect::<Vec<_>>(),
        "code_action: invocation",
    );
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
        } else if is_deprecated_interpolation(diag) {
            if let Some(action) =
                make_unwrap_interpolation_action(&uri, diag, &doc.rope)
            {
                actions.push(CodeActionOrCommand::CodeAction(action));
            }
        } else if is_deprecated_lookup(diag) {
            if let Some(action) =
                make_convert_lookup_to_index_action(&uri, diag, body, &doc.rope)
            {
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

    // Cursor-driven per-variable insert. Triggered when the cursor
    // sits ANYWHERE inside an untyped `variable "X" { … }` block,
    // not only when the diag's narrow `variable` keyword span is
    // in `params.context.diagnostics`. nvim only ships diags whose
    // ranges intersect the cursor, so an action gated purely on
    // the diag would never appear when the user is on the block
    // label or interior — even though the diag is firing.
    if let Some(action) = make_insert_variable_type_action_at_cursor(
        &uri,
        params.range.start,
        body,
        &doc.rope,
        &doc.symbols,
        &backend.state,
    ) {
        // Avoid stacking two identical actions when the diag-driven
        // path already produced one for the same variable name.
        let already = actions.iter().any(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == action.title,
            _ => false,
        });
        if !already {
            actions.push(CodeActionOrCommand::CodeAction(action));
        }
    }

    // "Refine `type = any`" — for every variable declared with
    // `type = any` whose default / caller signal yields a more
    // concrete shape, replace `any` with the inferred type.
    if let Some(action) =
        make_refine_any_types_action(&uri, body, &doc.rope, &doc.symbols, &backend.state)
    {
        actions.push(CodeActionOrCommand::CodeAction(action));
    }

    // "Generate variable declarations for undefined `var.X` references"
    // — walk the doc's references; for any `var.<name>` whose target
    // isn't declared in this module's symbol table, append a stub
    // `variable "<name>" {}` block at the end of the current file.
    if let Some(action) = make_declare_undefined_variables_action(
        &uri,
        &doc.rope,
        &doc.symbols,
        &doc.references,
    ) {
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

/// Match the `terraform_deprecated_interpolation` warning so we
/// can offer a rewrite that drops the `"${…}"` wrapper.
fn is_deprecated_interpolation(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::WARNING)
        && diag.source.as_deref() == Some("terraform-ls-rs")
        && diag
            .message
            .contains("interpolation-only expressions are deprecated")
}

/// Match the `terraform_deprecated_lookup` warning so we can offer
/// a rewrite from `lookup(X, "k")` (deprecated 2-arg form) to
/// `X["k"]` (index notation, type-agnostic).
fn is_deprecated_lookup(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::WARNING)
        && diag.source.as_deref() == Some("terraform-ls-rs")
        && diag
            .message
            .contains("two-argument `lookup()` is deprecated")
}

/// Quick-fix for `terraform_deprecated_lookup`. Rewrites
/// `lookup(X, "k")` to `X["k"]` — index notation, semantically
/// equivalent and valid for ANY collection type. We deliberately
/// do NOT rewrite to `X.k` even when the key is a valid identifier,
/// because:
///
/// - For `Object({k = …})` both `X.k` and `X["k"]` work.
/// - For `Map(T)` runtime maps where `k` is a runtime value,
///   `X.k` is a static error if `k` isn't a known field — we
///   can't tell at parse time.
///
/// `X["k"]` works in both cases. The user can hand-simplify to
/// `X.k` afterwards if they know `X` is an Object.
fn make_convert_lookup_to_index_action(
    uri: &Url,
    diag: &Diagnostic,
    body: &Body,
    rope: &Rope,
) -> Option<CodeAction> {
    use hcl_edit::expr::Expression;
    use hcl_edit::repr::Span as _;

    let diag_start = tfls_parser::lsp_position_to_byte_offset(rope, diag.range.start).ok()?;
    let diag_end = tfls_parser::lsp_position_to_byte_offset(rope, diag.range.end).ok()?;

    // Find the 2-arg `lookup` FuncCall whose span matches the diag range.
    let mut found: Option<(String, String)> = None;
    tfls_diag::expr_walk::for_each_expression(body, |expr| {
        if found.is_some() {
            return;
        }
        let Expression::FuncCall(call) = expr else { return };
        if !call.name.namespace.is_empty() {
            return;
        }
        if call.name.name.as_str() != "lookup" {
            return;
        }
        if call.args.iter().count() != 2 {
            return;
        }
        let Some(span) = call.span() else { return };
        if span.start != diag_start || span.end != diag_end {
            return;
        }
        let mut args = call.args.iter();
        let arg1 = args.next();
        let arg2 = args.next();
        let (Some(a1), Some(a2)) = (arg1, arg2) else { return };
        let (Some(s1), Some(s2)) = (a1.span(), a2.span()) else { return };
        let arg1_src = rope.byte_slice(s1.start..s1.end).to_string();
        let arg2_src = rope.byte_slice(s2.start..s2.end).to_string();
        found = Some((arg1_src, arg2_src));
    });

    let (arg1_src, arg2_src) = found?;
    let arg1_trim = arg1_src.trim().to_string();
    let arg2_trim = arg2_src.trim().to_string();
    let new_text = format!("{arg1_trim}[{arg2_trim}]");

    let edit = TextEdit {
        range: diag.range,
        new_text: new_text.clone(),
    };
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);
    Some(CodeAction {
        title: format!("Convert `lookup({arg1_trim}, {arg2_trim})` to `{new_text}`"),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diag.clone()]),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        // Index notation is semantics-preserving, but `X.k` may
        // be the user's preferred final form — leave the choice
        // open by not pinning `is_preferred = true`.
        is_preferred: None,
        ..Default::default()
    })
}

/// Quick-fix for `terraform_deprecated_interpolation`. Replaces
/// the entire `"${EXPR}"` slice with just `EXPR`. The diagnostic
/// range covers the whole string template, so we slice the rope
/// at that range, strip the leading `"${` (with optional
/// whitespace) and the trailing `}"`, and emit a `TextEdit` whose
/// `new_text` is the inner expression text.
fn make_unwrap_interpolation_action(
    uri: &Url,
    diag: &Diagnostic,
    rope: &Rope,
) -> Option<CodeAction> {
    let start = tfls_parser::lsp_position_to_byte_offset(rope, diag.range.start).ok()?;
    let end = tfls_parser::lsp_position_to_byte_offset(rope, diag.range.end).ok()?;
    if end <= start {
        return None;
    }
    let slice: String = rope.byte_slice(start..end).to_string();
    let trimmed = slice.trim();
    let dollar_brace = trimmed.find("${")?;
    let inner_start = dollar_brace + "${".len();
    // Templates can't nest `${…}` without literal text between, but
    // the inner expression CAN contain `}` (e.g. an object literal
    // `{a=1}`), so we balance braces forward from the opening.
    let bytes = trimmed.as_bytes();
    let mut depth = 1i32;
    let mut i = inner_start;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return None;
    }
    let inner = trimmed[inner_start..i].trim();
    if inner.is_empty() {
        return None;
    }

    let edit = TextEdit {
        range: diag.range,
        new_text: inner.to_string(),
    };
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);
    Some(CodeAction {
        title: format!("Unwrap interpolation: `{inner}`"),
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

/// Source-level action: walk every `var.<name>` reference in this
/// document and, for any `<name>` not declared as a `variable
/// "<name>"` block in the local module's symbol table, append a
/// stub `variable "<name>" {}` at end-of-file. Bulk version of
/// what a user would do by hand when an undefined-variable warning
/// fires; saves the per-block round-trip.
///
/// Only fires when at least one undeclared `var.X` reference exists
/// — otherwise no menu entry.
fn make_declare_undefined_variables_action(
    uri: &Url,
    rope: &Rope,
    symbols: &tfls_core::SymbolTable,
    references: &[tfls_parser::Reference],
) -> Option<CodeAction> {
    use std::collections::BTreeSet;
    use tfls_parser::ReferenceKind;

    let mut undeclared: BTreeSet<String> = BTreeSet::new();
    for r in references {
        if let ReferenceKind::Variable { name } = &r.kind {
            if !symbols.variables.contains_key(name) {
                undeclared.insert(name.clone());
            }
        }
    }
    if undeclared.is_empty() {
        return None;
    }

    let mut new_text = String::new();
    let total_bytes = rope.len_bytes();
    let last_char = if total_bytes == 0 {
        None
    } else {
        rope.byte_slice(total_bytes - 1..total_bytes)
            .to_string()
            .chars()
            .next()
    };
    if last_char != Some('\n') && total_bytes > 0 {
        new_text.push('\n');
    }
    new_text.push('\n');
    for name in &undeclared {
        new_text.push_str(&format!("variable \"{name}\" {{}}\n"));
    }

    let end_pos = tfls_parser::byte_offset_to_lsp_position(rope, total_bytes).ok()?;
    let edit = TextEdit {
        range: Range {
            start: end_pos,
            end: end_pos,
        },
        new_text,
    };
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    let count = undeclared.len();
    let names_preview: Vec<&str> = undeclared.iter().take(3).map(String::as_str).collect();
    let more_suffix = if count > 3 {
        format!(", … and {} more", count - 3)
    } else {
        String::new()
    };
    Some(CodeAction {
        title: format!(
            "Declare {count} undefined variable{plural}: {names}{more}",
            plural = if count == 1 { "" } else { "s" },
            names = names_preview.join(", "),
            more = more_suffix,
        ),
        kind: Some(CodeActionKind::SOURCE),
        diagnostics: None,
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        is_preferred: None,
        ..Default::default()
    })
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

/// Cursor-position variant of [`make_insert_variable_type_action`].
/// Walks the file for the `variable` block whose span contains
/// `cursor`; if it has no `type` attribute and inference yields a
/// concrete shape, builds the same `Set variable type` quick-fix
/// the diag-driven path produces. Used so the action surfaces from
/// anywhere inside the block — nvim only ships diagnostics whose
/// ranges intersect the cursor, and the typed-variables warning's
/// range is just the `variable` keyword.
fn make_insert_variable_type_action_at_cursor(
    uri: &Url,
    cursor: Position,
    body: &Body,
    rope: &Rope,
    symbols: &tfls_core::SymbolTable,
    state: &tfls_state::StateStore,
) -> Option<CodeAction> {
    use hcl_edit::repr::Span as _;

    // Find the variable block whose source span contains the cursor.
    let mut target: Option<(&Block, String)> = None;
    let mut block_count = 0usize;
    for structure in body.iter() {
        let Some(block) = structure.as_block() else { continue };
        if block.ident.as_str() != "variable" {
            continue;
        }
        block_count += 1;
        let Some(span) = block.span() else {
            tracing::info!(
                cursor_line = cursor.line,
                cursor_char = cursor.character,
                "infer-type at-cursor: variable block has no span",
            );
            continue;
        };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else { continue };
        let inside = contains(&range, cursor);
        tracing::info!(
            cursor_line = cursor.line,
            cursor_char = cursor.character,
            block_start = format!("{}:{}", range.start.line, range.start.character),
            block_end = format!("{}:{}", range.end.line, range.end.character),
            inside,
            "infer-type at-cursor: candidate block",
        );
        if !inside {
            continue;
        }
        let Some(name) = block.labels.first().and_then(label_str) else {
            continue;
        };
        target = Some((block, name.to_string()));
        break;
    }
    let Some((block, var_name)) = target else {
        tracing::info!(
            cursor_line = cursor.line,
            cursor_char = cursor.character,
            block_count,
            "infer-type at-cursor: no enclosing variable block found",
        );
        return None;
    };
    if block_has_attribute(block, "type") {
        tracing::info!(
            var = %var_name,
            "infer-type at-cursor: block already has type",
        );
        return None;
    }

    let inferred_from_default = symbols
        .variable_defaults
        .get(&var_name)
        .filter(|t| is_actionable_inference(t))
        .cloned();
    let inferred = inferred_from_default.or_else(|| {
        let module_dir = crate::handlers::util::parent_dir(uri)?;
        let merged = state.merged_assigned_type(&module_dir, &var_name)?;
        if !is_actionable_inference(&merged) {
            return None;
        }
        Some(merged)
    });

    let (insert_pos, prefix) = insertion_position(block, rope)?;

    // When no concrete inference is available (typical for
    // modules with no callers in the workspace), still offer a
    // `type = any` placeholder. The user always wants SOMETHING
    // to land at the cursor; sitting on an untyped block with
    // only the file-level fix-all option visible is confusing.
    // The placeholder is semantically safe (`any` matches
    // anything) and the existing "Refine `type = any`" source
    // action will replace it later if/when an inference signal
    // appears.
    let (rendered, title_source, is_placeholder) = match &inferred {
        Some(ty) => {
            let src = if symbols
                .variable_defaults
                .get(&var_name)
                .is_some_and(is_actionable_inference)
            {
                "default"
            } else {
                "tfvars / module callers"
            };
            (ty.to_string(), src, false)
        }
        None => ("any".to_string(), "no inference — adjust as needed", true),
    };

    let new_text = format!("{prefix}  type = {rendered}\n");
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
        title: format!("Set variable type to `{rendered}` ({title_source})"),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: None,
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        // Placeholder shouldn't auto-apply via "fix all"
        // shortcuts — we don't actually know the type.
        is_preferred: if is_placeholder {
            Some(false)
        } else {
            Some(matches!(
                inferred,
                Some(tfls_core::variable_type::VariableType::Primitive(_))
            ))
        },
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

/// Source-level action: walk every `variable` block whose `type =`
/// is the bare `any` (not parametrised — `list(any)` / `map(any)`
/// stay untouched since they're already specifying a collection
/// shape) and, if inference yields a more concrete shape, replace
/// `any` with the inferred type. Single combined `WorkspaceEdit`
/// covering all such blocks.
///
/// Suppressed when no variable qualifies — keeps the menu clean.
fn make_refine_any_types_action(
    uri: &Url,
    body: &Body,
    rope: &Rope,
    symbols: &tfls_core::SymbolTable,
    state: &tfls_state::StateStore,
) -> Option<CodeAction> {
    use hcl_edit::expr::Expression;
    use hcl_edit::repr::Span as _;

    let module_dir = crate::handlers::util::parent_dir(uri);
    let mut edits: Vec<TextEdit> = Vec::new();
    let mut refined_count = 0usize;

    for structure in body.iter() {
        let Some(block) = structure.as_block() else { continue };
        if block.ident.as_str() != "variable" {
            continue;
        }
        let Some(name) = block.labels.first().and_then(label_str) else {
            continue;
        };
        // Find the `type = …` attribute. Only proceed when the
        // value is the bare `any` identifier — `list(any)` etc are
        // already shape-specifying and not the target here.
        let mut type_attr_span: Option<std::ops::Range<usize>> = None;
        let mut type_value_span: Option<std::ops::Range<usize>> = None;
        for sub in block.body.iter() {
            let Some(attr) = sub.as_attribute() else { continue };
            if attr.key.as_str() != "type" {
                continue;
            }
            type_attr_span = attr.span();
            if let Expression::Variable(v) = &attr.value {
                if v.as_str() == "any" {
                    type_value_span = attr.value.span();
                }
            }
            break;
        }
        let Some(value_span) = type_value_span else { continue };
        let _ = type_attr_span; // reserved for future "remove `type =` line" variant

        // Inference: same priority order as the fix-all action.
        let inferred_from_default = symbols
            .variable_defaults
            .get(name)
            .filter(|t| is_actionable_inference(t))
            .filter(|t| !matches!(t, tfls_core::variable_type::VariableType::Any))
            .cloned();
        let inferred = inferred_from_default.or_else(|| {
            let dir = module_dir.as_deref()?;
            let merged = state.merged_assigned_type(dir, name)?;
            if !is_actionable_inference(&merged) {
                return None;
            }
            if matches!(&merged, tfls_core::variable_type::VariableType::Any) {
                return None;
            }
            Some(merged)
        });
        let Some(ty) = inferred else { continue };

        let Ok(start) = tfls_parser::byte_offset_to_lsp_position(rope, value_span.start) else {
            continue;
        };
        let Ok(end) = tfls_parser::byte_offset_to_lsp_position(rope, value_span.end) else {
            continue;
        };
        edits.push(TextEdit {
            range: Range { start, end },
            new_text: ty.to_string(),
        });
        refined_count += 1;
    }

    if edits.is_empty() {
        return None;
    }
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri.clone(), edits);
    Some(CodeAction {
        title: format!(
            "Refine `type = any` to inferred shape on {refined_count} variable{plural}",
            plural = if refined_count == 1 { "" } else { "s" },
        ),
        kind: Some(CodeActionKind::SOURCE),
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
