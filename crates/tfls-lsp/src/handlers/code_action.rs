//! `textDocument/codeAction` — quick fixes derived from our own
//! diagnostics.
//!
//! Currently provides one fix: insert any required attributes that a
//! resource block is missing.

use std::collections::{HashMap, HashSet};

use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, BlockLabel, Body};
use sonic_rs::JsonValueTrait;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
    Diagnostic, DiagnosticSeverity, Position, Range, TextEdit, Url, WorkspaceEdit,
};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;
use tfls_state::{DocumentState, StateStore};
use tower_lsp::jsonrpc;

use crate::backend::Backend;
use crate::handlers::code_action_scope::{
    Scope, build_scoped_action, for_each_doc_in_scope, range_intersects, range_is_empty,
};
use crate::handlers::util::module_supports_terraform_data;

pub async fn code_action(
    backend: &Backend,
    params: CodeActionParams,
) -> jsonrpc::Result<Option<CodeActionResponse>> {
    let uri = params.text_document.uri.clone();
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };

    let mut actions: Vec<CodeActionOrCommand> = Vec::new();

    tracing::info!(
        uri = %uri,
        diag_count = params.context.diagnostics.len(),
        body_some = doc.parsed.body.is_some(),
        diags = ?params.context.diagnostics.iter().map(|d| (
            d.severity, d.source.as_deref().unwrap_or(""), d.message.clone()
        )).collect::<Vec<_>>(),
        "code_action: invocation",
    );

    // Per-diag and cursor-driven Instance actions need a parsed
    // body; skip them if the active doc didn't parse, but DO fall
    // through to the scoped variants below — Module/Workspace
    // scope can still surface fixes from sibling docs that did
    // parse.
    if let Some(body) = doc.parsed.body.as_ref() {
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
            } else if is_deprecated_null_resource(diag) {
                if let Some(action) =
                    make_replace_null_resource_for_diag(&backend.state, &uri, diag, body, &doc.rope)
                {
                    actions.push(CodeActionOrCommand::CodeAction(action));
                }
            }
        }
    }

    // "Fix all" — single action that adds `type = …` to every
    // untyped variable in the file with inferable type. Surfaced
    // independently of `params.context.diagnostics` so the user
    // can invoke it from a generic source-action menu without
    // having to position the cursor on a specific diagnostic.
    if let Some(body) = doc.parsed.body.as_ref() {
        if let Some(action) =
            make_fix_all_variable_types_action(&uri, body, &doc.rope, &doc.symbols, &backend.state)
        {
            actions.push(CodeActionOrCommand::CodeAction(action));
        }
    }

    // Cursor-driven per-variable insert. Triggered when the cursor
    // sits ANYWHERE inside an untyped `variable "X" { … }` block,
    // not only when the diag's narrow `variable` keyword span is
    // in `params.context.diagnostics`. nvim only ships diags whose
    // ranges intersect the cursor, so an action gated purely on
    // the diag would never appear when the user is on the block
    // label or interior — even though the diag is firing.
    if let Some(body) = doc.parsed.body.as_ref() {
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
    }

    // Cursor-driven `data "template_file"` → `templatefile()` rewrite.
    if let Some(body) = doc.parsed.body.as_ref() {
        if let Some(action) = make_replace_template_file_at_cursor(
            &backend.state,
            &uri,
            params.range.start,
            body,
            &doc.rope,
        ) {
            let already = actions.iter().any(|a| match a {
                CodeActionOrCommand::CodeAction(ca) => ca.title == action.title,
                _ => false,
            });
            if !already {
                actions.push(CodeActionOrCommand::CodeAction(action));
            }
        }
    }

    // Cursor-driven `null_resource` → `terraform_data` rewrite.
    // Surfaces the Instance variant when the cursor sits inside
    // a `resource "null_resource" "X" { … }` block; broader
    // scopes are emitted below via `emit_scoped_actions`.
    if let Some(body) = doc.parsed.body.as_ref() {
        if module_supports_terraform_data(&backend.state, &uri) {
            if let Some(action) = make_replace_null_resource_at_cursor(
                &backend.state,
                &uri,
                params.range.start,
                body,
                &doc.rope,
            ) {
                let already = actions.iter().any(|a| match a {
                    CodeActionOrCommand::CodeAction(ca) => ca.title == action.title,
                    _ => false,
                });
                if !already {
                    actions.push(CodeActionOrCommand::CodeAction(action));
                }
            }
        }
    }

    // Drop the per-doc shard guard before iterating again under
    // `for_each_doc_in_scope` (which acquires its own guards on
    // `state.documents`).
    drop(doc);

    let selection = if range_is_empty(&params.range) {
        None
    } else {
        Some(params.range)
    };

    // Multi-scope variants. Each emit covers (Selection?) + File +
    // Module + Workspace. Per-doc scan returns 0+ TextEdits; the
    // helper assembles WorkspaceEdits + scope-tagged kinds + dedup.
    let state = &backend.state;
    emit_scoped_actions(
        state,
        &uri,
        selection,
        true,
        "Unwrap interpolation",
        "deprecated interpolation",
        "unwrap-interpolation",
        &mut actions,
        |_doc_uri, doc| {
            let Some(body) = doc.parsed.body.as_ref() else {
                return Vec::new();
            };
            scan_unwrap_interpolations(body, &doc.rope)
        },
    );
    emit_scoped_actions(
        state,
        &uri,
        selection,
        true,
        "Convert deprecated lookup to index notation",
        "deprecated lookup",
        "convert-lookup-to-index",
        &mut actions,
        |_doc_uri, doc| {
            let Some(body) = doc.parsed.body.as_ref() else {
                return Vec::new();
            };
            scan_lookup_to_index(body, &doc.rope)
        },
    );
    emit_scoped_actions(
        state,
        &uri,
        selection,
        true,
        "Set variable types",
        "untyped variable",
        "set-variable-types",
        &mut actions,
        |doc_uri, doc| {
            let Some(body) = doc.parsed.body.as_ref() else {
                return Vec::new();
            };
            scan_insert_variable_types(doc_uri, body, &doc.rope, &doc.symbols, state)
        },
    );
    emit_scoped_actions(
        state,
        &uri,
        selection,
        true,
        "Refine `type = any`",
        "`type = any` variable",
        "refine-any-types",
        &mut actions,
        |doc_uri, doc| {
            let Some(body) = doc.parsed.body.as_ref() else {
                return Vec::new();
            };
            scan_refine_any_types(doc_uri, body, &doc.rope, &doc.symbols, state)
        },
    );

    emit_null_resource_actions(state, &uri, selection, &mut actions);
    emit_template_file_actions(state, &uri, selection, &mut actions);

    // Declare undefined variables — File + Module only (the edit
    // appends to EOF, so Workspace would scatter stubs across
    // unrelated files; Selection is N/A for an EOF append).
    emit_declare_undefined_actions(state, &uri, &mut actions);

    // Move out-of-place outputs / variables into the module's
    // canonical files. Same standard-module-structure driver.
    emit_move_outputs_actions(state, &uri, &mut actions);
    emit_move_variables_actions(state, &uri, &mut actions);

    // Format the buffer / module / workspace under the active
    // `formatStyle`. Selection variant when the user has a
    // visual range. Skips files that are already formatted so
    // the menu only shows actionable entries.
    emit_format_actions(state, &uri, selection, &mut actions);

    if actions.is_empty() { Ok(None) } else { Ok(Some(actions)) }
}

/// Generic scope iterator for actions whose per-doc transform can
/// be expressed as a pure `(uri, doc) -> Vec<TextEdit>` scan.
///
/// Emits up to 4 `CodeAction`s: optional Selection, then File,
/// Module, optional Workspace. Each variant uses
/// [`build_scoped_action`] for title + LSP `CodeActionKind`
/// derivation, so the menu strings stay consistent across actions
/// (see `code_action_scope::scope_title`).
///
/// `scan` is called once per visited doc inside
/// [`for_each_doc_in_scope`]'s callback. The closure gets each
/// doc's URI and `DocumentState`; for Selection scope the
/// returned edits are filtered down to those whose range
/// intersects `selection`.
#[allow(clippy::too_many_arguments)]
fn emit_scoped_actions<F>(
    state: &StateStore,
    primary_uri: &Url,
    selection: Option<Range>,
    include_workspace: bool,
    title_template: &str,
    item_label: &str,
    action_id: &str,
    actions: &mut Vec<CodeActionOrCommand>,
    mut scan: F,
) where
    F: FnMut(&Url, &DocumentState) -> Vec<TextEdit>,
{
    let mut scopes: Vec<Scope> = Vec::new();
    if let Some(range) = selection {
        scopes.push(Scope::Selection { range });
    }
    scopes.extend([Scope::File, Scope::Module]);
    if include_workspace {
        scopes.push(Scope::Workspace);
    }

    // Per-doc scan cache. The scan callback is deterministic
    // per (uri, doc), so each scope only differs in its
    // post-filter (Selection range). Compute once, filter
    // multiple times.
    let mut scan_cache: HashMap<Url, Vec<TextEdit>> = HashMap::new();

    for scope in scopes {
        let mut edits_by_uri: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        let mut visited = 0usize;
        let mut total_edits = 0usize;
        for_each_doc_in_scope(state, primary_uri, scope, |doc_uri, doc| {
            visited += 1;
            if !scan_cache.contains_key(doc_uri) {
                scan_cache.insert(doc_uri.clone(), scan(doc_uri, doc));
            }
            let mut v = scan_cache
                .get(doc_uri)
                .cloned()
                .unwrap_or_default();
            if let Scope::Selection { range } = scope {
                v.retain(|e| range_intersects(&e.range, &range));
            }
            total_edits += v.len();
            if !v.is_empty() {
                edits_by_uri.insert(doc_uri.clone(), v);
            }
        });
        tracing::info!(
            action_id,
            scope = ?scope,
            visited,
            docs_with_edits = edits_by_uri.len(),
            total_edits,
            "scoped action scan",
        );
        if let Some(action) = build_scoped_action(
            scope,
            edits_by_uri,
            title_template,
            item_label,
            None,
            action_id,
        ) {
            actions.push(CodeActionOrCommand::CodeAction(action));
        }
    }
}

/// Scope iteration for `declare-undefined-variables`.
///
/// Always targets `<module-dir>/variables.tf` regardless of which
/// file the action was invoked from. Anything else trips
/// `terraform_standard_module_structure` — declaring variables in
/// `main.tf` (or any other file) is itself a flagged authoring
/// mistake. If the target file already exists (loaded or on
/// disk), we append at EOF; otherwise the WorkspaceEdit
/// includes a `CreateFile` op so the LSP client materialises it.
///
/// Module-scope only — File/Selection/Workspace don't fit:
/// - File: would imply per-source-doc routing, but we always
///   write to `variables.tf`. Single Module entry is enough.
/// - Selection: appends are at EOF, not in the selection.
/// - Workspace: stubs would scatter across unrelated modules.
fn emit_declare_undefined_actions(
    state: &StateStore,
    primary_uri: &Url,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    use crate::handlers::code_action_scope::{scope_kind, scope_title};

    let Some(module_dir) = crate::handlers::util::parent_dir(primary_uri) else {
        return;
    };

    // Undeclared = module-wide refs minus the union of every
    // sibling `.tf` file's declarations. A var declared in any
    // sibling counts as "declared" for the whole module.
    let declared = per_doc_declared_set(state, primary_uri, Scope::Module);
    let undeclared = collect_undeclared_names(state, primary_uri, Scope::Module, &declared);
    if undeclared.is_empty() {
        return;
    }

    let target_path = module_dir.join("variables.tf");
    let Ok(target_url) = Url::from_file_path(&target_path) else {
        return;
    };
    let strategy = resolve_target_strategy(state, &target_url, &target_path);
    let edit = build_declare_undefined_workspace_edit(&target_url, &strategy, &undeclared);

    let count = undeclared.len();
    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: scope_title(
            "Declare undefined variables",
            "undefined variable",
            Scope::Module,
            count,
        ),
        kind: Some(scope_kind(Scope::Module, "declare-undefined-variables")),
        diagnostics: None,
        edit: Some(edit),
        is_preferred: None,
        ..Default::default()
    }));
}

/// Build the union of variable declarations across the docs that
/// `scope` would visit. Used as the "declared" set so module-wide
/// declarations suppress what would otherwise look like undefined
/// references in a peer file.
fn per_doc_declared_set(
    state: &StateStore,
    primary_uri: &Url,
    scope: Scope,
) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for_each_doc_in_scope(state, primary_uri, scope, |_uri, doc| {
        for name in doc.symbols.variables.keys() {
            out.insert(name.clone());
        }
    });
    out
}

/// Distinct `var.X` reference names across `scope`'s docs that
/// aren't in `declared`. BTreeSet so the resulting `variable "X"
/// {}` stubs land in deterministic order.
fn collect_undeclared_names(
    state: &StateStore,
    primary_uri: &Url,
    scope: Scope,
    declared: &HashSet<String>,
) -> std::collections::BTreeSet<String> {
    use std::collections::BTreeSet;
    use tfls_parser::ReferenceKind;

    let mut out: BTreeSet<String> = BTreeSet::new();
    for_each_doc_in_scope(state, primary_uri, scope, |_uri, doc| {
        for r in &doc.references {
            if let ReferenceKind::Variable { name } = &r.kind {
                if !declared.contains(name) {
                    out.insert(name.clone());
                }
            }
        }
    });
    out
}

/// How we should reach `variables.tf` to insert the new stubs.
enum TargetFileStrategy {
    /// File is in `state.documents` — append at the rope's EOF.
    Loaded {
        eof: Position,
        needs_leading_newline: bool,
    },
    /// File exists on disk but isn't a tracked document — read it
    /// to compute the EOF position, then send a plain `TextEdit`.
    OnDisk {
        eof: Position,
        needs_leading_newline: bool,
    },
    /// File doesn't exist; emit a `CreateFile` op so the client
    /// materialises it before applying the inserts.
    Create,
}

fn resolve_target_strategy(
    state: &StateStore,
    target_url: &Url,
    target_path: &std::path::Path,
) -> TargetFileStrategy {
    if let Some(doc) = state.documents.get(target_url) {
        let total = doc.rope.len_bytes();
        let last_char = if total == 0 {
            None
        } else {
            doc.rope
                .byte_slice(total - 1..total)
                .to_string()
                .chars()
                .next()
        };
        let needs_leading_newline = total > 0 && last_char != Some('\n');
        let eof = tfls_parser::byte_offset_to_lsp_position(&doc.rope, total)
            .unwrap_or(Position::new(0, 0));
        return TargetFileStrategy::Loaded {
            eof,
            needs_leading_newline,
        };
    }
    let Ok(content) = std::fs::read_to_string(target_path) else {
        return TargetFileStrategy::Create;
    };
    let rope = ropey::Rope::from_str(&content);
    let total = rope.len_bytes();
    let needs_leading_newline = total > 0 && !content.ends_with('\n');
    let eof = tfls_parser::byte_offset_to_lsp_position(&rope, total)
        .unwrap_or(Position::new(0, 0));
    TargetFileStrategy::OnDisk {
        eof,
        needs_leading_newline,
    }
}

fn build_declare_undefined_workspace_edit(
    target_url: &Url,
    strategy: &TargetFileStrategy,
    undeclared: &std::collections::BTreeSet<String>,
) -> WorkspaceEdit {
    let mut blocks = String::new();
    for name in undeclared {
        blocks.push_str(&format!("variable \"{name}\" {{}}\n"));
    }

    match strategy {
        TargetFileStrategy::Loaded {
            eof,
            needs_leading_newline,
        }
        | TargetFileStrategy::OnDisk {
            eof,
            needs_leading_newline,
        } => {
            let mut text = String::new();
            if *needs_leading_newline {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(&blocks);
            let edit = TextEdit {
                range: Range {
                    start: *eof,
                    end: *eof,
                },
                new_text: text,
            };
            let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
            changes.insert(target_url.clone(), vec![edit]);
            WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }
        }
        TargetFileStrategy::Create => {
            use lsp_types::{
                CreateFile, CreateFileOptions, DocumentChangeOperation, DocumentChanges,
                OneOf, OptionalVersionedTextDocumentIdentifier, ResourceOp, TextDocumentEdit,
            };
            let create_op = DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                uri: target_url.clone(),
                options: Some(CreateFileOptions {
                    overwrite: Some(false),
                    ignore_if_exists: Some(true),
                }),
                annotation_id: None,
            }));
            let initial_edit = TextEdit {
                range: Range {
                    start: Position::new(0, 0),
                    end: Position::new(0, 0),
                },
                new_text: blocks,
            };
            let doc_edit = DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: target_url.clone(),
                    version: None,
                },
                edits: vec![OneOf::Left(initial_edit)],
            });
            WorkspaceEdit {
                document_changes: Some(DocumentChanges::Operations(vec![create_op, doc_edit])),
                ..Default::default()
            }
        }
    }
}

/// Move-outputs source-action: relocate every `output "X" { … }`
/// block in any sibling `.tf` file (other than `outputs.tf`) into
/// the module's `outputs.tf`. Mirror of declare-undefined's
/// "always target a canonical file" UX, driven by the same
/// `terraform_standard_module_structure` rule that flags outputs
/// living outside `outputs.tf`.
///
/// Module scope only. Skips entirely when the active doc isn't
/// part of a resolvable module dir, or when no out-of-place
/// outputs exist anywhere in the module.
fn emit_move_outputs_actions(
    state: &StateStore,
    primary_uri: &Url,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    use crate::handlers::code_action_scope::{scope_kind, scope_title};

    let Some(module_dir) = crate::handlers::util::parent_dir(primary_uri) else {
        return;
    };

    // Collect out-of-place output blocks across the module.
    // (uri, delete-range, source-text-of-block).
    let mut deletions: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    let mut moved_sources: Vec<String> = Vec::new();
    for_each_doc_in_scope(state, primary_uri, Scope::Module, |doc_uri, doc| {
        if filename_of(doc_uri).as_deref() == Some("outputs.tf") {
            return;
        }
        let Some(body) = doc.parsed.body.as_ref() else {
            return;
        };
        for (range, src) in scan_blocks_of_kind(body, &doc.rope, "output") {
            deletions
                .entry(doc_uri.clone())
                .or_default()
                .push(TextEdit {
                    range,
                    new_text: String::new(),
                });
            moved_sources.push(src);
        }
    });
    if moved_sources.is_empty() {
        return;
    }

    let target_path = module_dir.join("outputs.tf");
    let Ok(target_url) = Url::from_file_path(&target_path) else {
        return;
    };
    let strategy = resolve_target_strategy(state, &target_url, &target_path);
    let combined = combine_block_sources(&moved_sources);

    let workspace_edit = build_move_blocks_workspace_edit(
        &deletions,
        &target_url,
        &strategy,
        &combined,
    );

    let count = moved_sources.len();
    let title = scope_title(
        "Move output blocks",
        "output block",
        Scope::Module,
        count,
    );
    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("{title} (to `outputs.tf`)"),
        kind: Some(scope_kind(Scope::Module, "move-outputs-to-outputs-tf")),
        diagnostics: None,
        edit: Some(workspace_edit),
        is_preferred: None,
        ..Default::default()
    }));
}

/// Symmetric counterpart to `emit_move_outputs_actions`: lifts
/// every `variable "X" { … }` block from any sibling `.tf` file
/// (other than `variables.tf`) into the module's `variables.tf`.
/// Same `terraform_standard_module_structure` driver — that rule
/// flags variable declarations living outside `variables.tf`
/// just like it does for outputs.
fn emit_move_variables_actions(
    state: &StateStore,
    primary_uri: &Url,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    use crate::handlers::code_action_scope::{scope_kind, scope_title};

    let Some(module_dir) = crate::handlers::util::parent_dir(primary_uri) else {
        return;
    };

    let mut deletions: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    let mut moved_sources: Vec<String> = Vec::new();
    for_each_doc_in_scope(state, primary_uri, Scope::Module, |doc_uri, doc| {
        if filename_of(doc_uri).as_deref() == Some("variables.tf") {
            return;
        }
        let Some(body) = doc.parsed.body.as_ref() else {
            return;
        };
        for (range, src) in scan_blocks_of_kind(body, &doc.rope, "variable") {
            deletions
                .entry(doc_uri.clone())
                .or_default()
                .push(TextEdit {
                    range,
                    new_text: String::new(),
                });
            moved_sources.push(src);
        }
    });
    if moved_sources.is_empty() {
        return;
    }

    let target_path = module_dir.join("variables.tf");
    let Ok(target_url) = Url::from_file_path(&target_path) else {
        return;
    };
    let strategy = resolve_target_strategy(state, &target_url, &target_path);
    let combined = combine_block_sources(&moved_sources);

    let workspace_edit = build_move_blocks_workspace_edit(
        &deletions,
        &target_url,
        &strategy,
        &combined,
    );

    let count = moved_sources.len();
    let title = scope_title(
        "Move variable blocks",
        "variable block",
        Scope::Module,
        count,
    );
    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("{title} (to `variables.tf`)"),
        kind: Some(scope_kind(Scope::Module, "move-variables-to-variables-tf")),
        diagnostics: None,
        edit: Some(workspace_edit),
        is_preferred: None,
        ..Default::default()
    }));
}

/// Format a single document under the active style. Returns a
/// whole-file `TextEdit` when the formatted output differs from
/// the input; `None` when the doc is already formatted, the
/// rope is empty, or the formatter rejected the source (parse
/// error, etc).
///
/// Pure — no LSP-state access. Caller decides which docs to
/// scan and which style to use.
fn scan_format(
    rope: &Rope,
    style: tfls_state::FormatStyle,
) -> Option<TextEdit> {
    let text = rope.to_string();
    let formatted = tfls_format::format_source(&text, style).ok()?;
    if formatted == text {
        return None;
    }
    Some(TextEdit {
        range: crate::handlers::formatting::whole_document_range(rope),
        new_text: formatted,
    })
}

/// Format-as-code-action across scopes. Reads the live
/// `format_style` once at invocation; switching mid-action
/// would be confusing. Custom title format because the standard
/// `scope_title` counts edits-per-item, but format always
/// produces exactly one whole-file edit per file — so we report
/// the count of FILES that would change, not edits.
///
/// Action-id is `"format"`, producing kinds:
/// - `quickfix.terraform-ls-rs.format.selection`
/// - `source.fixAll.terraform-ls-rs.format` (File)
/// - `source.fixAll.terraform-ls-rs.format.module`
/// - `source.fixAll.terraform-ls-rs.format.workspace`
///
/// Each scope's branch only emits an action if at least one file
/// would actually change — keeps the menu clean on already-
/// formatted buffers.
fn emit_format_actions(
    state: &StateStore,
    primary_uri: &Url,
    selection: Option<Range>,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    use crate::handlers::code_action_scope::scope_kind;
    use crate::handlers::formatting::slice_text;

    let style = state.config.snapshot().format_style;

    // Selection scope — slice + format that range only.
    if let Some(range) = selection {
        if let Some(doc) = state.documents.get(primary_uri) {
            if let Some(slice) = slice_text(&doc.rope, range) {
                if let Ok(formatted) = tfls_format::format_source(&slice, style) {
                    if formatted != slice {
                        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
                        changes.insert(
                            primary_uri.clone(),
                            vec![TextEdit {
                                range,
                                new_text: formatted,
                            }],
                        );
                        let line_count = range
                            .end
                            .line
                            .saturating_sub(range.start.line)
                            + 1;
                        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                            title: format!("Format selection ({line_count} lines)"),
                            kind: Some(scope_kind(Scope::Selection { range }, "format")),
                            edit: Some(WorkspaceEdit {
                                changes: Some(changes),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }));
                    }
                }
            }
        }
    }

    // File / Module / Workspace — per-doc scan_format.
    for scope in [Scope::File, Scope::Module, Scope::Workspace] {
        let mut edits_by_uri: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for_each_doc_in_scope(state, primary_uri, scope, |doc_uri, doc| {
            if let Some(edit) = scan_format(&doc.rope, style) {
                edits_by_uri.insert(doc_uri.clone(), vec![edit]);
            }
        });
        if edits_by_uri.is_empty() {
            continue;
        }
        let count = edits_by_uri.len();
        let title = match scope {
            Scope::File => "Format file".to_string(),
            Scope::Module => format!(
                "Format {count} .tf file{} in this module",
                if count == 1 { "" } else { "s" }
            ),
            Scope::Workspace => format!(
                "Format {count} .tf file{} in workspace",
                if count == 1 { "" } else { "s" }
            ),
            // Selection / Instance handled above; for_each_doc_in_scope
            // never yields them in this loop's iteration set.
            _ => continue,
        };
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title,
            kind: Some(scope_kind(scope, "format")),
            edit: Some(WorkspaceEdit {
                changes: Some(edits_by_uri),
                ..Default::default()
            }),
            ..Default::default()
        }));
    }
}

/// Last path segment of a `file://` URI, e.g. `"main.tf"`.
fn filename_of(uri: &Url) -> Option<String> {
    let path = uri.to_file_path().ok()?;
    Some(path.file_name()?.to_string_lossy().into_owned())
}

/// Find every block of the given kind in `body` and return its
/// `(LSP delete-range, original source text)`. The delete-range
/// extends from the block's start through any trailing newline +
/// blank line so the cleanup leaves no double-blank-line scar.
fn scan_blocks_of_kind(body: &Body, rope: &Rope, kind: &str) -> Vec<(Range, String)> {
    use hcl_edit::repr::Span as _;

    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != kind {
            continue;
        }
        let Some(span) = block.span() else { continue };
        let total = rope.len_bytes();
        let mut end = span.end.min(total);
        // Pull in trailing whitespace + a single line break so the
        // remaining file isn't left with stranded blank lines
        // exactly where the block used to be.
        while end < total {
            let ch = rope
                .byte_slice(end..end + 1)
                .to_string()
                .chars()
                .next();
            match ch {
                Some('\n') => {
                    end += 1;
                    break;
                }
                Some(' ') | Some('\t') | Some('\r') => end += 1,
                _ => break,
            }
        }
        let Ok(start_pos) = tfls_parser::byte_offset_to_lsp_position(rope, span.start) else {
            continue;
        };
        let Ok(end_pos) = tfls_parser::byte_offset_to_lsp_position(rope, end) else {
            continue;
        };
        let src = rope.byte_slice(span.start..end).to_string();
        out.push((
            Range {
                start: start_pos,
                end: end_pos,
            },
            src,
        ));
    }
    out
}

/// Walk every `resource "null_resource" "X"` block in `body`
/// and emit the text edits that turn it into a `resource
/// "terraform_data" "X"` block:
///
/// - Replace the `"null_resource"` label literal with
///   `"terraform_data"` (preserving the surrounding quotes).
/// - For each `triggers` attribute inside the block, replace
///   the key with `triggers_replace`.
/// - Rewrite every `null_resource.X[.attr]` reference in the
///   body — head ident becomes `terraform_data`, `.triggers`
///   becomes `.triggers_replace`. Other attributes (`id`, …)
///   stay untouched.
fn scan_null_resource_to_terraform_data(body: &Body, rope: &Rope) -> Vec<TextEdit> {
    scan_null_resource_to_terraform_data_for(body, rope, None)
}

/// Like [`scan_null_resource_to_terraform_data`] but limited to
/// resources whose name (`labels.get(1)`) is in `names`. Used
/// by the cursor / diag-attached Instance variants so a single-
/// block conversion doesn't drag references to *other*
/// `null_resource` blocks (which the user hasn't converted yet)
/// into its edit set.
fn scan_null_resource_to_terraform_data_for(
    body: &Body,
    rope: &Rope,
    names: Option<&HashSet<String>>,
) -> Vec<TextEdit> {
    use hcl_edit::repr::Span as _;

    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "resource" {
            continue;
        }
        let Some(label) = block.labels.first() else {
            continue;
        };
        if label_str(label) != Some("null_resource") {
            continue;
        }
        if let Some(filter) = names {
            let block_name = block.labels.get(1).and_then(label_str).unwrap_or("");
            if !filter.contains(block_name) {
                continue;
            }
        }

        // 1. Block label rewrite.
        if let Some(span) = label.span() {
            if let (Ok(start), Ok(end)) = (
                tfls_parser::byte_offset_to_lsp_position(rope, span.start),
                tfls_parser::byte_offset_to_lsp_position(rope, span.end),
            ) {
                out.push(TextEdit {
                    range: Range { start, end },
                    new_text: "\"terraform_data\"".into(),
                });
            }
        }

        // 2. `triggers` → `triggers_replace` attribute rename.
        for sub in block.body.iter() {
            let Some(attr) = sub.as_attribute() else {
                continue;
            };
            if attr.key.as_str() != "triggers" {
                continue;
            }
            if let Some(span) = attr.key.span() {
                if let (Ok(start), Ok(end)) = (
                    tfls_parser::byte_offset_to_lsp_position(rope, span.start),
                    tfls_parser::byte_offset_to_lsp_position(rope, span.end),
                ) {
                    out.push(TextEdit {
                        range: Range { start, end },
                        new_text: "triggers_replace".into(),
                    });
                }
            }
        }
    }

    // 3. Reference rewriting — every `null_resource.X[.attr]`
    // traversal in the body becomes `terraform_data.X[.attr]`,
    // with `.triggers` upgraded to `.triggers_replace`. Skips
    // `null_resource.X.id` and other attrs that survive on
    // `terraform_data` unchanged. Walks the entire body so it
    // catches references inside `locals`, `output`, attribute
    // values, dynamic blocks, templates, etc.
    visit_body_for_null_resource_refs(body, rope, names, &mut out);

    out
}

/// Recursively walk `body`, emitting reference-rewrite edits for
/// every `null_resource.<X>[.attr]` traversal encountered. When
/// `names` is `Some`, only references to those names are
/// rewritten (Instance-variant filter).
fn visit_body_for_null_resource_refs(
    body: &Body,
    rope: &Rope,
    names: Option<&HashSet<String>>,
    out: &mut Vec<TextEdit>,
) {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            visit_expr_for_null_resource_refs(&attr.value, rope, names, out);
        } else if let Some(block) = structure.as_block() {
            visit_body_for_null_resource_refs(&block.body, rope, names, out);
        }
    }
}

fn visit_expr_for_null_resource_refs(
    expr: &hcl_edit::expr::Expression,
    rope: &Rope,
    names: Option<&HashSet<String>>,
    out: &mut Vec<TextEdit>,
) {
    use hcl_edit::expr::{Expression as Ex, TraversalOperator};
    use hcl_edit::repr::Span as _;

    match expr {
        Ex::Traversal(t) => {
            if let Ex::Variable(v) = &t.expr {
                if v.as_str() == "null_resource" {
                    // Pull the resource name (first GetAttr) and
                    // confirm it passes the optional name filter
                    // before emitting any edits.
                    let res_name = t.operators.iter().find_map(|op| match op.value() {
                        TraversalOperator::GetAttr(ident) => Some(ident.as_str().to_string()),
                        _ => None,
                    });
                    let in_filter = match (names, res_name.as_deref()) {
                        (None, _) => true,
                        (Some(_), None) => false,
                        (Some(filter), Some(n)) => filter.contains(n),
                    };
                    if in_filter {
                        if let Some(span) = v.span() {
                            if let (Ok(start), Ok(end)) = (
                                tfls_parser::byte_offset_to_lsp_position(rope, span.start),
                                tfls_parser::byte_offset_to_lsp_position(rope, span.end),
                            ) {
                                out.push(TextEdit {
                                    range: Range { start, end },
                                    new_text: "terraform_data".into(),
                                });
                            }
                        }
                        // The attr accessor (after the resource
                        // name) is the *second* GetAttr.
                        let mut idx = 0usize;
                        for op in t.operators.iter() {
                            if let TraversalOperator::GetAttr(ident) = op.value() {
                                idx += 1;
                                if idx == 2 && ident.as_str() == "triggers" {
                                    if let Some(span) = ident.span() {
                                        if let (Ok(start), Ok(end)) = (
                                            tfls_parser::byte_offset_to_lsp_position(
                                                rope, span.start,
                                            ),
                                            tfls_parser::byte_offset_to_lsp_position(
                                                rope, span.end,
                                            ),
                                        ) {
                                            out.push(TextEdit {
                                                range: Range { start, end },
                                                new_text: "triggers_replace".into(),
                                            });
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            visit_expr_for_null_resource_refs(&t.expr, rope, names, out);
            for op in t.operators.iter() {
                if let TraversalOperator::Index(e) = op.value() {
                    visit_expr_for_null_resource_refs(e, rope, names, out);
                }
            }
        }
        Ex::Array(a) => {
            for e in a.iter() {
                visit_expr_for_null_resource_refs(e, rope, names, out);
            }
        }
        Ex::Object(o) => {
            for (_k, v) in o.iter() {
                visit_expr_for_null_resource_refs(v.expr(), rope, names, out);
            }
        }
        Ex::FuncCall(f) => {
            for arg in f.args.iter() {
                visit_expr_for_null_resource_refs(arg, rope, names, out);
            }
        }
        Ex::Parenthesis(p) => visit_expr_for_null_resource_refs(p.inner(), rope, names, out),
        Ex::UnaryOp(u) => visit_expr_for_null_resource_refs(&u.expr, rope, names, out),
        Ex::BinaryOp(b) => {
            visit_expr_for_null_resource_refs(&b.lhs_expr, rope, names, out);
            visit_expr_for_null_resource_refs(&b.rhs_expr, rope, names, out);
        }
        Ex::Conditional(c) => {
            visit_expr_for_null_resource_refs(&c.cond_expr, rope, names, out);
            visit_expr_for_null_resource_refs(&c.true_expr, rope, names, out);
            visit_expr_for_null_resource_refs(&c.false_expr, rope, names, out);
        }
        Ex::ForExpr(f) => {
            visit_expr_for_null_resource_refs(&f.intro.collection_expr, rope, names, out);
            if let Some(k) = f.key_expr.as_ref() {
                visit_expr_for_null_resource_refs(k, rope, names, out);
            }
            visit_expr_for_null_resource_refs(&f.value_expr, rope, names, out);
            if let Some(c) = f.cond.as_ref() {
                visit_expr_for_null_resource_refs(&c.expr, rope, names, out);
            }
        }
        Ex::StringTemplate(t) => {
            visit_template_for_null_resource_refs(t.iter(), rope, names, out)
        }
        Ex::HeredocTemplate(h) => {
            visit_template_for_null_resource_refs(h.template.iter(), rope, names, out)
        }
        _ => {}
    }
}

fn visit_template_for_null_resource_refs<'a, I>(
    elements: I,
    rope: &Rope,
    names: Option<&HashSet<String>>,
    out: &mut Vec<TextEdit>,
) where
    I: IntoIterator<Item = &'a hcl_edit::template::Element>,
{
    use hcl_edit::template::{Directive, Element};
    for element in elements {
        match element {
            Element::Literal(_) => {}
            Element::Interpolation(i) => {
                visit_expr_for_null_resource_refs(&i.expr, rope, names, out)
            }
            Element::Directive(d) => match d.as_ref() {
                Directive::If(i) => {
                    visit_expr_for_null_resource_refs(&i.if_expr.cond_expr, rope, names, out);
                    visit_template_for_null_resource_refs(
                        i.if_expr.template.iter(),
                        rope,
                        names,
                        out,
                    );
                    if let Some(else_part) = i.else_expr.as_ref() {
                        visit_template_for_null_resource_refs(
                            else_part.template.iter(),
                            rope,
                            names,
                            out,
                        );
                    }
                }
                Directive::For(f) => {
                    visit_expr_for_null_resource_refs(
                        &f.for_expr.collection_expr,
                        rope,
                        names,
                        out,
                    );
                    visit_template_for_null_resource_refs(
                        f.for_expr.template.iter(),
                        rope,
                        names,
                        out,
                    );
                }
            },
        }
    }
}

/// Diag-attached Instance variant: builds the
/// `null_resource` → `terraform_data` rewrite for a specific
/// `terraform_deprecated_null_resource` warning that nvim has
/// shipped in `params.context.diagnostics`. Locates the block
/// by the diag range (the diag's range covers the
/// `"null_resource"` label literal).
fn make_replace_null_resource_for_diag(
    state: &StateStore,
    uri: &Url,
    diag: &Diagnostic,
    body: &Body,
    rope: &Rope,
) -> Option<CodeAction> {
    use hcl_edit::repr::Span as _;

    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "resource" {
            continue;
        }
        let Some(label) = block.labels.first() else {
            continue;
        };
        if label_str(label) != Some("null_resource") {
            continue;
        }
        let Some(label_span) = label.span() else { continue };
        let Ok(label_range) = hcl_span_to_lsp_range(rope, label_span) else {
            continue;
        };
        if label_range.start != diag.range.start {
            continue;
        }
        let name = block.labels.get(1).and_then(label_str).unwrap_or("?");
        let mut filter = HashSet::new();
        filter.insert(name.to_string());
        let edits = scan_null_resource_to_terraform_data_for(body, rope, Some(&filter));
        if edits.is_empty() {
            return None;
        }
        let mut rewrites: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        rewrites.insert(uri.clone(), edits);
        let mut names_by_module: HashMap<std::path::PathBuf, Vec<String>> = HashMap::new();
        if let Some(dir) = crate::handlers::util::parent_dir(uri) {
            names_by_module.insert(dir, vec![name.to_string()]);
        }
        let workspace_edit =
            build_null_resource_workspace_edit(state, rewrites, names_by_module);
        return Some(CodeAction {
            title: format!("Convert null_resource.{name} to terraform_data"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diag.clone()]),
            edit: Some(workspace_edit),
            is_preferred: Some(true),
            ..Default::default()
        });
    }
    None
}

/// Names (`labels.get(1)`) of every `resource "null_resource"
/// "X"` block in `body`. Used to drive the moved-block
/// generator: each name becomes a `moved { from =
/// null_resource.X to = terraform_data.X }` block in `moved.tf`
/// alongside the rewrite, so Terraform migrates state in place
/// instead of destroy+create.
fn null_resource_names_in_body(body: &Body) -> Vec<String> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "resource" {
            continue;
        }
        if block.labels.first().and_then(label_str) != Some("null_resource") {
            continue;
        }
        if let Some(name) = block.labels.get(1).and_then(label_str) {
            out.push(name.to_string());
        }
    }
    out
}

/// Names already covered by a `moved { from = null_resource.X
/// to = terraform_data.X }` block in `body`. The generator
/// skips these so re-running the action on a partially migrated
/// module is idempotent.
fn existing_null_resource_moved_names(body: &Body) -> HashSet<String> {
    let mut out = HashSet::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "moved" {
            continue;
        }
        let from = traversal_attr_string(&block.body, "from");
        let to = traversal_attr_string(&block.body, "to");
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        let Some(name) = from.strip_prefix("null_resource.") else {
            continue;
        };
        if to == format!("terraform_data.{name}") {
            out.insert(name.to_string());
        }
    }
    out
}

/// Read a `<ident>.<ident>` traversal stored on attribute `key`
/// inside `body` (`from = null_resource.foo` →
/// `Some("null_resource.foo")`). Returns `None` for any other
/// expression form (literal strings, complex expressions, ...).
fn traversal_attr_string(body: &Body, key: &str) -> Option<String> {
    use hcl_edit::expr::Expression;
    for sub in body.iter() {
        let Some(attr) = sub.as_attribute() else {
            continue;
        };
        if attr.key.as_str() != key {
            continue;
        }
        if let Expression::Traversal(t) = &attr.value {
            use hcl_edit::expr::TraversalOperator;
            let head = match &t.expr {
                Expression::Variable(v) => v.as_str().to_string(),
                _ => return None,
            };
            let mut acc = head;
            for op in t.operators.iter() {
                let v = op.value();
                match v {
                    TraversalOperator::GetAttr(d) => {
                        acc.push('.');
                        acc.push_str(d.as_str());
                    }
                    _ => return None,
                }
            }
            return Some(acc);
        }
    }
    None
}

/// Format a single `moved { from = null_resource.X to =
/// terraform_data.X }` block source. Trailing newline included
/// so callers can concatenate without separators.
fn format_moved_block(name: &str) -> String {
    format!(
        "moved {{\n  from = null_resource.{name}\n  to   = terraform_data.{name}\n}}\n"
    )
}

/// Scope iteration for `null-resource-to-terraform-data`.
/// Custom (vs `emit_scoped_actions`) because the standard
/// title counts EDITS and each block produces 1+ edits — we
/// want the title to count BLOCKS instead.
///
/// Per-doc gate lookup: a `null_resource` block is only offered
/// for conversion when the *enclosing module* admits Terraform
/// 1.4+. We cache the decision per module dir so a Workspace-
/// scope sweep doesn't re-walk siblings for every visited doc.
///
/// Bundles a `moved.tf` companion per module: each converted
/// `null_resource.X` gets a `moved { from = null_resource.X to
/// = terraform_data.X }` block written to the module's
/// `moved.tf` (created if absent). Without these blocks
/// Terraform plans the rewrite as destroy+create — `moved`
/// migrates state in place.
fn emit_null_resource_actions(
    state: &StateStore,
    primary_uri: &Url,
    selection: Option<Range>,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    use crate::handlers::code_action_scope::scope_kind;
    use std::path::PathBuf;

    let mut scopes: Vec<Scope> = Vec::new();
    if let Some(range) = selection {
        scopes.push(Scope::Selection { range });
    }
    scopes.extend([Scope::File, Scope::Module, Scope::Workspace]);

    let mut module_gate_cache: HashMap<PathBuf, bool> = HashMap::new();
    // Per-doc scan cache. Workspace iteration in the bench's
    // single-doc fixture used to walk the same body 4× (once
    // per scope); now once. For multi-doc workspaces the
    // savings still scale linearly with scope count.
    //
    // Stored value: (full edit set, full block-name list).
    // `None` marker means "computed and gated out / empty".
    type ScanRow = Option<(Vec<TextEdit>, Vec<String>)>;
    let mut scan_cache: HashMap<Url, ScanRow> = HashMap::new();

    for scope in scopes {
        let mut edits_by_uri: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        let mut names_by_module: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut total_blocks = 0usize;
        for_each_doc_in_scope(state, primary_uri, scope, |doc_uri, doc| {
            // Compute-or-fetch from cache.
            if !scan_cache.contains_key(doc_uri) {
                let row = (|| -> ScanRow {
                    let body = doc.parsed.body.as_ref()?;
                    if let Some(dir) = crate::handlers::util::parent_dir(doc_uri) {
                        let supports = *module_gate_cache
                            .entry(dir)
                            .or_insert_with(|| module_supports_terraform_data(state, doc_uri));
                        if !supports {
                            return None;
                        }
                    } else if !module_supports_terraform_data(state, doc_uri) {
                        return None;
                    }
                    let edits = scan_null_resource_to_terraform_data(body, &doc.rope);
                    if edits.is_empty() {
                        return None;
                    }
                    Some((edits, null_resource_names_in_body(body)))
                })();
                scan_cache.insert(doc_uri.clone(), row);
            }
            let Some(Some((cached_edits, cached_names))) = scan_cache.get(doc_uri) else {
                return;
            };
            let mut v = cached_edits.clone();
            if let Scope::Selection { range } = scope {
                v.retain(|e| range_intersects(&e.range, &range));
            }
            if v.is_empty() {
                return;
            }
            let blocks = if matches!(scope, Scope::Selection { .. }) {
                v.iter().filter(|e| e.new_text == "\"terraform_data\"").count()
            } else {
                cached_names.len()
            };
            total_blocks += blocks;
            // Selection scope filters names by which blocks have
            // their label-rewrite edit in `v`; broader scopes
            // take the cached full-doc list verbatim.
            let names_in_scope: Vec<String> = if matches!(scope, Scope::Selection { .. }) {
                let body = match doc.parsed.body.as_ref() {
                    Some(b) => b,
                    None => return,
                };
                names_intersecting_edits(body, &v)
            } else {
                cached_names.clone()
            };
            edits_by_uri.insert(doc_uri.clone(), v);
            if let Some(dir) = crate::handlers::util::parent_dir(doc_uri) {
                names_by_module.entry(dir).or_default().extend(names_in_scope);
            }
        });
        if edits_by_uri.is_empty() || total_blocks == 0 {
            continue;
        }
        let plural = if total_blocks == 1 { "" } else { "s" };
        let where_ = match scope {
            Scope::Selection { .. } => "selection",
            Scope::File => "this file",
            Scope::Module => "this module",
            Scope::Workspace => "workspace",
            Scope::Instance => continue,
        };
        let title = format!(
            "Convert {total_blocks} null_resource block{plural} in {where_}"
        );
        let workspace_edit =
            build_null_resource_workspace_edit(state, edits_by_uri, names_by_module);
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title,
            kind: Some(scope_kind(scope, "null-resource-to-terraform-data")),
            edit: Some(workspace_edit),
            ..Default::default()
        }));
    }
}

/// Names of `null_resource` blocks whose label-rewrite edit
/// lies inside the selected sub-set of edits (`v`). Used to
/// keep the moved.tf companion in sync with selection-scoped
/// rewrites.
fn names_intersecting_edits(body: &Body, edits: &[TextEdit]) -> Vec<String> {
    use hcl_edit::repr::Span as _;
    let label_ranges: Vec<Range> = edits
        .iter()
        .filter(|e| e.new_text == "\"terraform_data\"")
        .map(|e| e.range)
        .collect();
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "resource" {
            continue;
        }
        if block.labels.first().and_then(label_str) != Some("null_resource") {
            continue;
        }
        let Some(name) = block.labels.get(1).and_then(label_str) else {
            continue;
        };
        let Some(label_span) = block.labels.first().and_then(|l| l.span()) else {
            continue;
        };
        let Ok(label_range) =
            tfls_parser::hcl_span_to_lsp_range(&ropey::Rope::from_str(""), label_span)
        else {
            // `hcl_span_to_lsp_range` needs a real rope — fall
            // through to compare against the edit ranges via
            // start position only.
            if label_ranges
                .iter()
                .any(|r| (r.start.line, r.start.character) == (0, 0))
            {
                out.push(name.to_string());
            }
            continue;
        };
        if label_ranges.iter().any(|r| r.start == label_range.start) {
            out.push(name.to_string());
        }
    }
    out
}

/// Build the `WorkspaceEdit` for a null_resource action given
/// the per-doc rewrite edits and per-module list of converted
/// names. Always uses `document_changes` because `moved.tf` may
/// need creating, and LSP forbids mixing `changes` with
/// `documentChanges`.
fn build_null_resource_workspace_edit(
    state: &StateStore,
    rewrites: HashMap<Url, Vec<TextEdit>>,
    names_by_module: HashMap<std::path::PathBuf, Vec<String>>,
) -> WorkspaceEdit {
    use lsp_types::{
        CreateFile, CreateFileOptions, DocumentChangeOperation, DocumentChanges, OneOf,
        OptionalVersionedTextDocumentIdentifier, ResourceOp, TextDocumentEdit,
    };

    let mut ops: Vec<DocumentChangeOperation> = Vec::new();

    // 1. Per-module `moved.tf` builder. Group, dedupe, drop
    // names already covered by an existing `moved` block in any
    // sibling — keeps the action idempotent.
    for (module_dir, mut names) in names_by_module {
        names.sort();
        names.dedup();
        let existing = collect_existing_moved_names(state, &module_dir);
        let to_add: Vec<String> = names
            .into_iter()
            .filter(|n| !existing.contains(n))
            .collect();
        if to_add.is_empty() {
            continue;
        }
        let target_path = module_dir.join("moved.tf");
        let Ok(target_url) = Url::from_file_path(&target_path) else {
            continue;
        };
        let strategy = resolve_target_strategy(state, &target_url, &target_path);
        let mut body_text = String::new();
        for n in &to_add {
            body_text.push_str(&format_moved_block(n));
        }
        match strategy {
            TargetFileStrategy::Loaded {
                eof,
                needs_leading_newline,
            }
            | TargetFileStrategy::OnDisk {
                eof,
                needs_leading_newline,
            } => {
                let mut text = String::new();
                if needs_leading_newline {
                    text.push('\n');
                }
                text.push('\n');
                text.push_str(&body_text);
                ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: target_url,
                        version: None,
                    },
                    edits: vec![OneOf::Left(TextEdit {
                        range: Range {
                            start: eof,
                            end: eof,
                        },
                        new_text: text,
                    })],
                }));
            }
            TargetFileStrategy::Create => {
                ops.push(DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: target_url.clone(),
                    options: Some(CreateFileOptions {
                        overwrite: Some(false),
                        ignore_if_exists: Some(true),
                    }),
                    annotation_id: None,
                })));
                ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: target_url,
                        version: None,
                    },
                    edits: vec![OneOf::Left(TextEdit {
                        range: Range {
                            start: Position::new(0, 0),
                            end: Position::new(0, 0),
                        },
                        new_text: body_text,
                    })],
                }));
            }
        }
    }

    // 2. Rewrites — append after moved.tf ops so a client that
    // applies in order writes the new file before any rename
    // touches it. (LSP doesn't actually guarantee order, but
    // most clients are sequential.)
    for (uri, edits) in rewrites {
        ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
            edits: edits.into_iter().map(OneOf::Left).collect(),
        }));
    }

    WorkspaceEdit {
        document_changes: Some(DocumentChanges::Operations(ops)),
        ..Default::default()
    }
}

/// Names already covered by `moved { from = null_resource.X
/// to = terraform_data.X }` blocks anywhere in `module_dir`.
fn collect_existing_moved_names(
    state: &StateStore,
    module_dir: &std::path::Path,
) -> HashSet<String> {
    let mut out = HashSet::new();
    for entry in state.documents.iter() {
        let uri = entry.key();
        let Ok(path) = uri.to_file_path() else { continue };
        if path.parent() != Some(module_dir) {
            continue;
        }
        let doc = entry.value();
        let Some(body) = doc.parsed.body.as_ref() else {
            continue;
        };
        out.extend(existing_null_resource_moved_names(body));
    }
    out
}

/// Cursor-driven Instance variant of the
/// `null_resource` → `terraform_data` rewrite. Surfaces only
/// when the cursor sits inside a `resource "null_resource" "X"`
/// block; broader scopes are emitted via `emit_scoped_actions`.
fn make_replace_null_resource_at_cursor(
    state: &StateStore,
    uri: &Url,
    cursor: Position,
    body: &Body,
    rope: &Rope,
) -> Option<CodeAction> {
    use hcl_edit::repr::Span as _;

    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "resource" {
            continue;
        }
        let Some(label) = block.labels.first() else {
            continue;
        };
        if label_str(label) != Some("null_resource") {
            continue;
        }
        let Some(span) = block.span() else { continue };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        if !contains(&range, cursor) {
            continue;
        }

        let name = block.labels.get(1).and_then(label_str).unwrap_or("?");
        let mut filter = HashSet::new();
        filter.insert(name.to_string());
        // Name-filtered scan: only edits for THIS block + its
        // own references; any other `null_resource.Y` blocks
        // and references to them stay untouched.
        let edits = scan_null_resource_to_terraform_data_for(body, rope, Some(&filter));
        if edits.is_empty() {
            return None;
        }

        let mut rewrites: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        rewrites.insert(uri.clone(), edits);
        let mut names_by_module: HashMap<std::path::PathBuf, Vec<String>> = HashMap::new();
        if let Some(dir) = crate::handlers::util::parent_dir(uri) {
            names_by_module.insert(dir, vec![name.to_string()]);
        }
        let workspace_edit =
            build_null_resource_workspace_edit(state, rewrites, names_by_module);
        return Some(CodeAction {
            title: format!("Convert null_resource.{name} to terraform_data"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: None,
            edit: Some(workspace_edit),
            is_preferred: Some(true),
            ..Default::default()
        });
    }
    None
}

/// Concatenate moved block source texts. Each entry from
/// `scan_output_blocks` already has a trailing newline (we
/// expanded the delete range past one newline), so plain
/// concatenation gives a clean result.
fn combine_block_sources(blocks: &[String]) -> String {
    let mut out = String::new();
    for b in blocks {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        // Insert a single blank line between blocks so the result
        // is readable even if individual block sources didn't have
        // trailing whitespace beyond the closing brace newline.
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(b);
    }
    out
}

/// Combine deletions across source files with the target write
/// (append-or-create). Always uses `documentChanges` so we can
/// mix `CreateFile` ops with `TextDocumentEdit`s — clients that
/// support documentChanges (which all major LSP clients do)
/// honour the order, applying the create before its initial
/// edit.
fn build_move_blocks_workspace_edit(
    deletions: &HashMap<Url, Vec<TextEdit>>,
    target_url: &Url,
    strategy: &TargetFileStrategy,
    combined: &str,
) -> WorkspaceEdit {
    use lsp_types::{
        CreateFile, CreateFileOptions, DocumentChangeOperation, DocumentChanges, OneOf,
        OptionalVersionedTextDocumentIdentifier, ResourceOp, TextDocumentEdit,
    };

    let mut ops: Vec<DocumentChangeOperation> = Vec::new();

    // Source file deletions (one TextDocumentEdit per source URI).
    for (uri, edits) in deletions {
        if edits.is_empty() {
            continue;
        }
        ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version: None,
            },
            edits: edits
                .iter()
                .cloned()
                .map(OneOf::Left)
                .collect(),
        }));
    }

    // Target outputs.tf: append or create-then-write.
    match strategy {
        TargetFileStrategy::Loaded {
            eof,
            needs_leading_newline,
        }
        | TargetFileStrategy::OnDisk {
            eof,
            needs_leading_newline,
        } => {
            let mut text = String::new();
            if *needs_leading_newline {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(combined);
            let append = TextEdit {
                range: Range {
                    start: *eof,
                    end: *eof,
                },
                new_text: text,
            };
            ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: target_url.clone(),
                    version: None,
                },
                edits: vec![OneOf::Left(append)],
            }));
        }
        TargetFileStrategy::Create => {
            ops.push(DocumentChangeOperation::Op(ResourceOp::Create(
                CreateFile {
                    uri: target_url.clone(),
                    options: Some(CreateFileOptions {
                        overwrite: Some(false),
                        ignore_if_exists: Some(true),
                    }),
                    annotation_id: None,
                },
            )));
            let initial = TextEdit {
                range: Range {
                    start: Position::new(0, 0),
                    end: Position::new(0, 0),
                },
                new_text: combined.to_string(),
            };
            ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: target_url.clone(),
                    version: None,
                },
                edits: vec![OneOf::Left(initial)],
            }));
        }
    }

    WorkspaceEdit {
        document_changes: Some(DocumentChanges::Operations(ops)),
        ..Default::default()
    }
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
/// Match the `terraform_deprecated_null_resource` warning so we
/// can attach the rewrite action directly to the diagnostic.
fn is_deprecated_null_resource(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::WARNING)
        && diag.source.as_deref() == Some("terraform-ls-rs")
        && diag
            .message
            .contains("`null_resource` is superseded by the built-in `terraform_data`")
}

fn is_deprecated_lookup(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::WARNING)
        && diag.source.as_deref() == Some("terraform-ls-rs")
        && diag
            .message
            .contains("two-argument `lookup()` is deprecated")
}

/// Compute the `(arg1_src, arg2_src)` pair for the 2-arg `lookup`
/// FuncCall whose span matches `(start, end)`. Shared between the
/// per-diag instance action and `scan_lookup_to_index`.
fn lookup_args_at(
    body: &Body,
    rope: &Rope,
    start: usize,
    end: usize,
) -> Option<(String, String)> {
    use hcl_edit::expr::Expression;
    use hcl_edit::repr::Span as _;

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
        if span.start != start || span.end != end {
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
    found
}

/// Compute the `lookup(X, k)` → `X[k]` rewrite for the call whose
/// span matches `range`. Shared between the per-diag instance
/// action and the multi-scope per-doc scan.
fn compute_lookup_to_index_edit(
    body: &Body,
    rope: &Rope,
    range: Range,
) -> Option<TextEdit> {
    let start = tfls_parser::lsp_position_to_byte_offset(rope, range.start).ok()?;
    let end = tfls_parser::lsp_position_to_byte_offset(rope, range.end).ok()?;
    let (arg1_src, arg2_src) = lookup_args_at(body, rope, start, end)?;
    let new_text = format!("{}[{}]", arg1_src.trim(), arg2_src.trim());
    Some(TextEdit { range, new_text })
}

/// Walk every deprecated 2-arg lookup() in `body`, return one
/// `TextEdit` per call. Built on top of the shared diagnostic
/// walker so action and warning stay aligned.
fn scan_lookup_to_index(body: &Body, rope: &Rope) -> Vec<TextEdit> {
    tfls_diag::deprecated_lookup_diagnostics(body, rope)
        .into_iter()
        .filter_map(|diag| compute_lookup_to_index_edit(body, rope, diag.range))
        .collect()
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
    let edit = compute_lookup_to_index_edit(body, rope, diag.range)?;
    let new_text = edit.new_text.clone();
    let start = tfls_parser::lsp_position_to_byte_offset(rope, diag.range.start).ok()?;
    let end = tfls_parser::lsp_position_to_byte_offset(rope, diag.range.end).ok()?;
    let (arg1_src, arg2_src) = lookup_args_at(body, rope, start, end)?;
    let arg1_trim = arg1_src.trim();
    let arg2_trim = arg2_src.trim();
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

/// Compute the `TextEdit` that replaces a `"${EXPR}"` slice
/// covered by `range` with just `EXPR`. Returns `None` when the
/// slice doesn't actually look like a whole-string interpolation
/// (e.g. unbalanced braces or empty inner expression).
///
/// Shared between the per-diagnostic instance action and the
/// per-doc scan used by file/module/workspace scope variants.
fn compute_unwrap_interpolation_edit(rope: &Rope, range: Range) -> Option<TextEdit> {
    let start = tfls_parser::lsp_position_to_byte_offset(rope, range.start).ok()?;
    let end = tfls_parser::lsp_position_to_byte_offset(rope, range.end).ok()?;
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
    Some(TextEdit {
        range,
        new_text: inner.to_string(),
    })
}

/// Walk every deprecated interpolation in `body`, return one
/// `TextEdit` per occurrence. Built on top of the existing
/// `tfls_diag::deprecated_interpolation_diagnostics` walker so
/// the action and the diagnostic stay in lock-step on what
/// counts as "deprecated".
fn scan_unwrap_interpolations(body: &Body, rope: &Rope) -> Vec<TextEdit> {
    tfls_diag::deprecated_interpolation_diagnostics(body, rope)
        .into_iter()
        .filter_map(|diag| compute_unwrap_interpolation_edit(rope, diag.range))
        .collect()
}

/// Per-diagnostic single-instance action — used when a specific
/// `terraform_deprecated_interpolation` warning is in
/// `params.context.diagnostics`. Emits the
/// `Unwrap interpolation: \`EXPR\`` quickfix with the original
/// diagnostic attached.
fn make_unwrap_interpolation_action(
    uri: &Url,
    diag: &Diagnostic,
    rope: &Rope,
) -> Option<CodeAction> {
    let edit = compute_unwrap_interpolation_edit(rope, diag.range)?;
    let inner = edit.new_text.clone();
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

/// Walk every untyped `variable` block in `body` and emit a
/// `type = <inferred>` insertion edit for each one with a
/// concrete inferable shape. Returns one edit per variable.
///
/// Powers both the legacy file-scope `source.fixAll` action and
/// the multi-scope (selection / file / module / workspace)
/// variants. Module/workspace iteration just changes WHICH docs
/// get scanned; the per-doc inference is identical.
fn scan_insert_variable_types(
    uri: &Url,
    body: &Body,
    rope: &Rope,
    symbols: &tfls_core::SymbolTable,
    state: &tfls_state::StateStore,
) -> Vec<TextEdit> {
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
    edits
}

/// Legacy file-scope `source.fixAll` — kept so the existing
/// `source.fixAll.terraform-ls-rs` kind remains stable for
/// clients filtering by it. The scoped variant
/// (`source.fixAll.terraform-ls-rs.set-variable-types`) is
/// emitted separately by `emit_scoped_actions`.
fn make_fix_all_variable_types_action(
    uri: &Url,
    body: &Body,
    rope: &Rope,
    symbols: &tfls_core::SymbolTable,
    state: &tfls_state::StateStore,
) -> Option<CodeAction> {
    let edits = scan_insert_variable_types(uri, body, rope, symbols, state);
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

/// Walk every `variable` block whose `type =` is the bare `any`
/// (not parametrised — `list(any)` / `map(any)` stay untouched
/// since they're already specifying a collection shape) and,
/// if inference yields a more concrete shape, emit an edit
/// replacing `any` with the inferred type.
fn scan_refine_any_types(
    uri: &Url,
    body: &Body,
    rope: &Rope,
    symbols: &tfls_core::SymbolTable,
    state: &tfls_state::StateStore,
) -> Vec<TextEdit> {
    use hcl_edit::expr::Expression;
    use hcl_edit::repr::Span as _;

    let module_dir = crate::handlers::util::parent_dir(uri);
    let mut edits: Vec<TextEdit> = Vec::new();

    for structure in body.iter() {
        let Some(block) = structure.as_block() else { continue };
        if block.ident.as_str() != "variable" {
            continue;
        }
        let Some(name) = block.labels.first().and_then(label_str) else {
            continue;
        };
        let mut type_value_span: Option<std::ops::Range<usize>> = None;
        for sub in block.body.iter() {
            let Some(attr) = sub.as_attribute() else { continue };
            if attr.key.as_str() != "type" {
                continue;
            }
            if let Expression::Variable(v) = &attr.value {
                if v.as_str() == "any" {
                    type_value_span = attr.value.span();
                }
            }
            break;
        }
        let Some(value_span) = type_value_span else { continue };

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
    }

    edits
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

// ── data "template_file" → templatefile() ─────────────────────────

/// Per-data-block conversion target.
#[derive(Debug, Clone)]
struct TemplateFileTarget {
    name: String,
    /// Range of the entire `data "template_file" "X" { ... }`
    /// block, expanded through any trailing newline so the
    /// deletion leaves no double-blank-line scar.
    delete_range: Range,
    /// Source text of the `template = ...` attribute value
    /// expression. Required (Terraform requires it on this
    /// data source); blocks without it are skipped (broken
    /// syntax — no point converting).
    template_src: String,
    /// Source text of the `vars = ...` value, or `{}` when
    /// absent.
    vars_src: String,
    /// EOF position in the host doc — where the new `local`
    /// is appended.
    eof: Position,
    /// Whether the host doc needs a leading newline before the
    /// appended `locals { }` block (file doesn't end with `\n`).
    needs_leading_newline: bool,
}

/// Per-doc scan for `data "template_file"` conversions. Returns
/// one [`TemplateFileTarget`] per convertible data block — the
/// caller decides which docs / scopes to apply them in.
fn scan_template_file_targets(rope: &Rope, body: &Body) -> Vec<TemplateFileTarget> {
    use hcl_edit::repr::Span as _;

    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "data" {
            continue;
        }
        if block.labels.first().and_then(label_str) != Some("template_file") {
            continue;
        }
        let Some(name) = block.labels.get(1).and_then(label_str) else {
            continue;
        };
        let Some(block_span) = block.span() else { continue };

        let template_src = match template_argument_source(rope, &block.body) {
            Some(s) => s,
            None => continue, // malformed input — skip
        };
        let vars_src = attribute_source(rope, &block.body, "vars")
            .unwrap_or_else(|| "{}".to_string());

        // Expand the delete range past one trailing newline so
        // we don't leave an empty line where the block was.
        let mut end = block_span.end;
        let total = rope.len_bytes();
        if end < total {
            let next = rope.byte_slice(end..end + 1).to_string();
            if next == "\n" {
                end += 1;
            }
        }
        let Ok(start_pos) = tfls_parser::byte_offset_to_lsp_position(rope, block_span.start)
        else {
            continue;
        };
        let Ok(end_pos) = tfls_parser::byte_offset_to_lsp_position(rope, end) else {
            continue;
        };

        let total_bytes = rope.len_bytes();
        let last_char = if total_bytes == 0 {
            None
        } else {
            rope.byte_slice(total_bytes - 1..total_bytes)
                .to_string()
                .chars()
                .next()
        };
        let needs_leading_newline = total_bytes > 0 && last_char != Some('\n');
        let eof = tfls_parser::byte_offset_to_lsp_position(rope, total_bytes)
            .unwrap_or(Position::new(0, 0));

        out.push(TemplateFileTarget {
            name: name.to_string(),
            delete_range: Range {
                start: start_pos,
                end: end_pos,
            },
            template_src,
            vars_src,
            eof,
            needs_leading_newline,
        });
    }
    out
}

/// Read the source text of attribute `key`'s value expression
/// from `rope` directly, preserving original formatting (heredocs,
/// multi-line objects, function calls, …).
fn attribute_source(rope: &Rope, body: &Body, key: &str) -> Option<String> {
    use hcl_edit::repr::Span as _;
    for sub in body.iter() {
        let Some(attr) = sub.as_attribute() else {
            continue;
        };
        if attr.key.as_str() != key {
            continue;
        }
        let span = attr.value.span()?;
        return Some(rope.byte_slice(span.start..span.end).to_string());
    }
    None
}

/// Pull the source text for the `template = ...` attribute,
/// unwrapping a `file(<path>)` wrapper if present. The
/// `template_file` data source semantically takes a *string*
/// rendered from `template`; users who pre-load that string
/// from disk write `template = file("path.tpl")`. The native
/// `templatefile()` function takes the *path* directly, so a
/// naive splice would produce
/// `templatefile(file("path.tpl"), …)` — `file()` reads the
/// raw bytes, defeating the function's whole purpose.
///
/// Detect the `file(<arg>)` shape and splice just `<arg>`'s
/// source. Inline literals + arbitrary expressions splice
/// verbatim.
fn template_argument_source(rope: &Rope, body: &Body) -> Option<String> {
    use hcl_edit::expr::Expression;
    use hcl_edit::repr::Span as _;
    for sub in body.iter() {
        let Some(attr) = sub.as_attribute() else {
            continue;
        };
        if attr.key.as_str() != "template" {
            continue;
        }
        // `file(<arg>)` with the bare-name `file` (no namespace)
        // and exactly one positional argument.
        if let Expression::FuncCall(call) = &attr.value {
            if call.name.namespace.is_empty()
                && call.name.name.as_str() == "file"
                && call.args.iter().count() == 1
            {
                if let Some(arg) = call.args.iter().next() {
                    let span = arg.span()?;
                    return Some(rope.byte_slice(span.start..span.end).to_string());
                }
            }
        }
        let span = attr.value.span()?;
        return Some(rope.byte_slice(span.start..span.end).to_string());
    }
    None
}

/// Build the file-level edits that turn `targets` into the
/// equivalent `templatefile()` calls in `host_uri`. Each target
/// emits two edits in the host doc: a delete of the data block
/// and an EOF append of `locals { name = templatefile(...) }`.
///
/// Returns `(edits, names_converted)` so the caller can plumb
/// the names through the reference-rewrite path.
fn template_file_host_edits(targets: &[TemplateFileTarget]) -> (Vec<TextEdit>, Vec<String>) {
    let mut edits = Vec::new();
    let mut names = Vec::new();
    if targets.is_empty() {
        return (edits, names);
    }

    // Aggregate the locals into ONE appended block per host
    // doc, keeping the file tidy. EOF + leading-newline state is
    // identical across all targets in the same doc.
    let eof = targets[0].eof;
    let needs_leading_newline = targets[0].needs_leading_newline;

    // 1. Deletes — one per target.
    for t in targets {
        edits.push(TextEdit {
            range: t.delete_range,
            new_text: String::new(),
        });
        names.push(t.name.clone());
    }

    // 2. Single appended `locals { ... }` block at EOF.
    let mut block = String::new();
    if needs_leading_newline {
        block.push('\n');
    }
    block.push('\n');
    block.push_str("locals {\n");
    for t in targets {
        block.push_str(&format!(
            "  {} = templatefile({}, {})\n",
            t.name,
            t.template_src.trim(),
            t.vars_src.trim(),
        ));
    }
    block.push_str("}\n");
    edits.push(TextEdit {
        range: Range {
            start: eof,
            end: eof,
        },
        new_text: block,
    });

    (edits, names)
}

/// Walk `body` and emit edits that rewrite every
/// `data.template_file.X.rendered` traversal (where `X ∈ names`)
/// into `local.X`. The `.rendered` accessor is *required* on
/// `data.template_file` — references that omit it are invalid
/// Terraform anyway.
fn template_file_reference_edits(
    body: &Body,
    rope: &Rope,
    names: &HashSet<String>,
    out: &mut Vec<TextEdit>,
) {
    if names.is_empty() {
        return;
    }
    visit_body_for_template_file_refs(body, rope, names, out);
}

fn visit_body_for_template_file_refs(
    body: &Body,
    rope: &Rope,
    names: &HashSet<String>,
    out: &mut Vec<TextEdit>,
) {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            visit_expr_for_template_file_refs(&attr.value, rope, names, out);
        } else if let Some(block) = structure.as_block() {
            visit_body_for_template_file_refs(&block.body, rope, names, out);
        }
    }
}

fn visit_expr_for_template_file_refs(
    expr: &hcl_edit::expr::Expression,
    rope: &Rope,
    names: &HashSet<String>,
    out: &mut Vec<TextEdit>,
) {
    use hcl_edit::expr::{Expression as Ex, TraversalOperator};
    use hcl_edit::repr::Span as _;

    match expr {
        Ex::Traversal(t) => {
            // Match `data.template_file.<X>.rendered`.
            if let Ex::Variable(v) = &t.expr {
                if v.as_str() == "data" {
                    let mut idx = 0usize;
                    let mut kind: Option<&str> = None;
                    let mut name: Option<&str> = None;
                    let mut rendered_seen = false;
                    for op in t.operators.iter() {
                        if let TraversalOperator::GetAttr(ident) = op.value() {
                            idx += 1;
                            match idx {
                                1 => kind = Some(ident.as_str()),
                                2 => name = Some(ident.as_str()),
                                3 => rendered_seen = ident.as_str() == "rendered",
                                _ => break,
                            }
                        } else {
                            break;
                        }
                    }
                    if kind == Some("template_file")
                        && rendered_seen
                        && name.is_some_and(|n| names.contains(n))
                    {
                        // Replace the entire traversal span with `local.<name>`.
                        if let (Some(span), Some(n)) = (t.span(), name) {
                            if let (Ok(start), Ok(end)) = (
                                tfls_parser::byte_offset_to_lsp_position(rope, span.start),
                                tfls_parser::byte_offset_to_lsp_position(rope, span.end),
                            ) {
                                out.push(TextEdit {
                                    range: Range { start, end },
                                    new_text: format!("local.{n}"),
                                });
                                return;
                            }
                        }
                    }
                }
            }
            visit_expr_for_template_file_refs(&t.expr, rope, names, out);
            for op in t.operators.iter() {
                if let TraversalOperator::Index(e) = op.value() {
                    visit_expr_for_template_file_refs(e, rope, names, out);
                }
            }
        }
        Ex::Array(a) => {
            for e in a.iter() {
                visit_expr_for_template_file_refs(e, rope, names, out);
            }
        }
        Ex::Object(o) => {
            for (_k, v) in o.iter() {
                visit_expr_for_template_file_refs(v.expr(), rope, names, out);
            }
        }
        Ex::FuncCall(f) => {
            for arg in f.args.iter() {
                visit_expr_for_template_file_refs(arg, rope, names, out);
            }
        }
        Ex::Parenthesis(p) => visit_expr_for_template_file_refs(p.inner(), rope, names, out),
        Ex::UnaryOp(u) => visit_expr_for_template_file_refs(&u.expr, rope, names, out),
        Ex::BinaryOp(b) => {
            visit_expr_for_template_file_refs(&b.lhs_expr, rope, names, out);
            visit_expr_for_template_file_refs(&b.rhs_expr, rope, names, out);
        }
        Ex::Conditional(c) => {
            visit_expr_for_template_file_refs(&c.cond_expr, rope, names, out);
            visit_expr_for_template_file_refs(&c.true_expr, rope, names, out);
            visit_expr_for_template_file_refs(&c.false_expr, rope, names, out);
        }
        Ex::ForExpr(f) => {
            visit_expr_for_template_file_refs(&f.intro.collection_expr, rope, names, out);
            if let Some(k) = f.key_expr.as_ref() {
                visit_expr_for_template_file_refs(k, rope, names, out);
            }
            visit_expr_for_template_file_refs(&f.value_expr, rope, names, out);
            if let Some(c) = f.cond.as_ref() {
                visit_expr_for_template_file_refs(&c.expr, rope, names, out);
            }
        }
        _ => {}
    }
}

/// Names already declared in any `locals { ... }` block under
/// `module_dir`. Used to skip conversions that would otherwise
/// produce a "Duplicate local value definition" Terraform
/// error: a `data "template_file" "x"` converted next to an
/// existing `local.x` would collide.
fn collect_existing_local_names(
    state: &StateStore,
    module_dir: &std::path::Path,
) -> HashSet<String> {
    let mut out = HashSet::new();
    for entry in state.documents.iter() {
        let uri = entry.key();
        let Ok(path) = uri.to_file_path() else { continue };
        if path.parent() != Some(module_dir) {
            continue;
        }
        let doc = entry.value();
        let Some(body) = doc.parsed.body.as_ref() else {
            continue;
        };
        for structure in body.iter() {
            let Some(block) = structure.as_block() else {
                continue;
            };
            if block.ident.as_str() != "locals" {
                continue;
            }
            for sub in block.body.iter() {
                if let Some(attr) = sub.as_attribute() {
                    out.insert(attr.key.as_str().to_string());
                }
            }
        }
    }
    out
}

/// Scope-aware emit for `template-file-to-templatefile`. Per
/// scope:
/// - collect convertible data blocks from each doc in scope
/// - per host doc: deletes + locals append
/// - per OTHER doc in module: reference rewrites
///
/// Per-module gate (templatefile is 0.12+) cached across docs.
/// Conversions whose name collides with an existing `local.X`
/// in the module are skipped (Terraform forbids duplicate
/// local definitions).
fn emit_template_file_actions(
    state: &StateStore,
    primary_uri: &Url,
    selection: Option<Range>,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    use crate::handlers::code_action_scope::scope_kind;
    use crate::handlers::util::module_supports_templatefile;
    use std::path::PathBuf;

    let mut scopes: Vec<Scope> = Vec::new();
    if let Some(range) = selection {
        scopes.push(Scope::Selection { range });
    }
    scopes.extend([Scope::File, Scope::Module, Scope::Workspace]);

    let mut module_gate_cache: HashMap<PathBuf, bool> = HashMap::new();
    let mut module_locals_cache: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    // Per-doc scan cache (module dir + already-collision-filtered
    // targets). One walk per doc per code_action call regardless
    // of scope count.
    let mut targets_cache: HashMap<Url, Option<(PathBuf, Vec<TemplateFileTarget>)>> =
        HashMap::new();
    // Per-(uri, sorted name set) ref-walk cache — same names
    // across File/Module/Workspace scopes hit one walk. Names
    // sorted so the key is canonical regardless of insertion
    // order from the per-doc names_set.
    let mut ref_edits_cache: HashMap<(Url, Vec<String>), Vec<TextEdit>> = HashMap::new();

    for scope in scopes {
        // Pass 1 — collect convertible targets per doc, gated.
        let mut targets_by_doc: HashMap<Url, Vec<TemplateFileTarget>> = HashMap::new();
        let mut names_by_module: HashMap<PathBuf, HashSet<String>> = HashMap::new();
        let mut total_blocks = 0usize;

        for_each_doc_in_scope(state, primary_uri, scope, |doc_uri, doc| {
            if !targets_cache.contains_key(doc_uri) {
                let row = (|| -> Option<(PathBuf, Vec<TemplateFileTarget>)> {
                    let body = doc.parsed.body.as_ref()?;
                    let dir = crate::handlers::util::parent_dir(doc_uri)?;
                    let supports = *module_gate_cache
                        .entry(dir.clone())
                        .or_insert_with(|| module_supports_templatefile(state, doc_uri));
                    if !supports {
                        return None;
                    }
                    let existing_locals = module_locals_cache
                        .entry(dir.clone())
                        .or_insert_with(|| collect_existing_local_names(state, &dir));
                    let mut targets = scan_template_file_targets(&doc.rope, body);
                    targets.retain(|t| !existing_locals.contains(&t.name));
                    if targets.is_empty() {
                        return None;
                    }
                    Some((dir, targets))
                })();
                targets_cache.insert(doc_uri.clone(), row);
            }
            let Some(Some((dir, cached_targets))) = targets_cache.get(doc_uri) else {
                return;
            };
            let mut targets = cached_targets.clone();
            if let Scope::Selection { range } = scope {
                targets.retain(|t| range_intersects(&t.delete_range, &range));
            }
            if targets.is_empty() {
                return;
            }
            total_blocks += targets.len();
            let names_set: HashSet<String> =
                targets.iter().map(|t| t.name.clone()).collect();
            names_by_module
                .entry(dir.clone())
                .or_default()
                .extend(names_set);
            targets_by_doc.insert(doc_uri.clone(), targets);
        });

        if targets_by_doc.is_empty() {
            continue;
        }

        // Pass 2 — build per-doc edit lists.
        let mut edits_by_uri: HashMap<Url, Vec<TextEdit>> = HashMap::new();

        // Host-doc edits.
        for (uri, targets) in &targets_by_doc {
            let (host_edits, _names) = template_file_host_edits(targets);
            edits_by_uri.entry(uri.clone()).or_default().extend(host_edits);
        }

        // Per-doc delete ranges (for filtering refs that fall
        // inside soon-to-be-deleted blocks — LSP rejects
        // overlapping edits). Empty for docs that contribute no
        // host edits (refs-only sweep).
        let delete_ranges_by_uri: HashMap<Url, Vec<Range>> = targets_by_doc
            .iter()
            .map(|(uri, targets)| {
                (
                    uri.clone(),
                    targets.iter().map(|t| t.delete_range).collect(),
                )
            })
            .collect();

        // Reference rewrites — for every doc in each affected
        // module, scan + emit `local.X` rewrites for the names
        // converted in that module. Filter out rewrites that
        // overlap a pending delete in the same doc — for example
        // `data.template_file.X { vars = data.template_file.Y.rendered }`
        // would otherwise emit a `local.Y` rewrite *inside* the
        // delete range of X. The deletion swallows the original
        // text either way; the filter just keeps the edit set
        // legal.
        for (module_dir, names) in &names_by_module {
            // Canonical sorted-names key for this scope's
            // converted set in this module. Re-used between
            // scopes that converge on the same name set.
            let mut names_key: Vec<String> = names.iter().cloned().collect();
            names_key.sort();

            for entry in state.documents.iter() {
                let uri = entry.key();
                let Ok(path) = uri.to_file_path() else { continue };
                if path.parent() != Some(module_dir) {
                    continue;
                }
                let cache_key = (uri.clone(), names_key.clone());
                if !ref_edits_cache.contains_key(&cache_key) {
                    let doc = entry.value();
                    let computed = doc
                        .parsed
                        .body
                        .as_ref()
                        .map(|body| {
                            let mut v = Vec::new();
                            template_file_reference_edits(body, &doc.rope, names, &mut v);
                            v
                        })
                        .unwrap_or_default();
                    ref_edits_cache.insert(cache_key.clone(), computed);
                }
                let mut ref_edits = ref_edits_cache
                    .get(&cache_key)
                    .cloned()
                    .unwrap_or_default();
                if ref_edits.is_empty() {
                    continue;
                }
                if let Some(deletes) = delete_ranges_by_uri.get(uri) {
                    ref_edits.retain(|e| {
                        !deletes.iter().any(|d| range_intersects(d, &e.range))
                    });
                }
                if let Scope::Selection { range } = scope {
                    ref_edits.retain(|e| range_intersects(&e.range, &range));
                }
                if ref_edits.is_empty() {
                    continue;
                }
                edits_by_uri
                    .entry(uri.clone())
                    .or_default()
                    .extend(ref_edits);
            }
        }

        if edits_by_uri.is_empty() {
            continue;
        }

        let plural = if total_blocks == 1 { "" } else { "s" };
        let where_ = match scope {
            Scope::Selection { .. } => "selection",
            Scope::File => "this file",
            Scope::Module => "this module",
            Scope::Workspace => "workspace",
            Scope::Instance => continue,
        };
        let title = format!(
            "Convert {total_blocks} template_file data block{plural} to templatefile() in {where_}"
        );
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title,
            kind: Some(scope_kind(scope, "template-file-to-templatefile")),
            edit: Some(WorkspaceEdit {
                changes: Some(edits_by_uri),
                ..Default::default()
            }),
            ..Default::default()
        }));
    }
}

/// Cursor-driven Instance variant: the user is inside a single
/// `data "template_file" "X"` block. Convert that one block (+
/// references to it).
fn make_replace_template_file_at_cursor(
    state: &StateStore,
    uri: &Url,
    cursor: Position,
    body: &Body,
    rope: &Rope,
) -> Option<CodeAction> {
    use crate::handlers::util::module_supports_templatefile;
    use hcl_edit::repr::Span as _;

    if !module_supports_templatefile(state, uri) {
        return None;
    }

    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "data" {
            continue;
        }
        if block.labels.first().and_then(label_str) != Some("template_file") {
            continue;
        }
        let Some(span) = block.span() else { continue };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        if !contains(&range, cursor) {
            continue;
        }

        // Reuse the per-doc scan, then keep only the matching
        // target.
        let name = block.labels.get(1).and_then(label_str)?.to_string();
        // Refuse on collision with an existing `local.<name>` —
        // Terraform errors on duplicate local definitions.
        if let Some(dir) = crate::handlers::util::parent_dir(uri) {
            if collect_existing_local_names(state, &dir).contains(&name) {
                return None;
            }
        }
        let mut targets = scan_template_file_targets(rope, body);
        targets.retain(|t| t.name == name);
        if targets.is_empty() {
            return None;
        }
        let (host_edits, _) = template_file_host_edits(&targets);
        // Capture delete ranges for this doc so refs that fall
        // inside the deleted block (e.g. self-reference in
        // vars) get filtered — LSP rejects overlapping edits.
        let host_delete_ranges: Vec<Range> =
            targets.iter().map(|t| t.delete_range).collect();

        // Refs: only this name, throughout the module.
        let mut filter = HashSet::new();
        filter.insert(name.clone());
        let module_dir = crate::handlers::util::parent_dir(uri);
        let mut edits_by_uri: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        edits_by_uri.insert(uri.clone(), host_edits);

        if let Some(dir) = module_dir.as_deref() {
            for entry in state.documents.iter() {
                let other_uri = entry.key();
                let Ok(path) = other_uri.to_file_path() else { continue };
                if path.parent() != Some(dir) {
                    continue;
                }
                let other_doc = entry.value();
                let Some(other_body) = other_doc.parsed.body.as_ref() else {
                    continue;
                };
                let mut ref_edits = Vec::new();
                template_file_reference_edits(
                    other_body,
                    &other_doc.rope,
                    &filter,
                    &mut ref_edits,
                );
                // Filter out ref edits inside the host doc's
                // pending delete range — overlap would corrupt
                // the LSP edit set. Only relevant for the host
                // doc itself.
                if other_uri == uri {
                    ref_edits.retain(|e| {
                        !host_delete_ranges
                            .iter()
                            .any(|d| range_intersects(d, &e.range))
                    });
                }
                if !ref_edits.is_empty() {
                    edits_by_uri
                        .entry(other_uri.clone())
                        .or_default()
                        .extend(ref_edits);
                }
            }
        }

        return Some(CodeAction {
            title: format!("Convert template_file.{name} to templatefile()"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: None,
            edit: Some(WorkspaceEdit {
                changes: Some(edits_by_uri),
                ..Default::default()
            }),
            is_preferred: Some(true),
            ..Default::default()
        });
    }
    None
}

/// Match the `terraform_deprecated_template_file` warning so the
/// corresponding `template_file_names_in_body`-aware quickfix
/// can surface alongside the diag.
#[allow(dead_code)]
fn is_deprecated_template_file(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::WARNING)
        && diag
            .message
            .contains("`data \"template_file\"` is superseded by the built-in `templatefile()`")
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
