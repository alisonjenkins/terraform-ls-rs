//! Navigation handlers: goto-definition, find-references, hover.

use hcl_edit::expr::{Expression, TraversalOperator};
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams, Location,
    MarkupContent, MarkupKind, Position, ReferenceParams, Url,
};
use tfls_core::SymbolKind;
use tfls_parser::lsp_position_to_byte_offset;
use tfls_state::{DocumentState, SymbolKey, reference_at_position, reference_key};
use tower_lsp::jsonrpc;

use crate::backend::Backend;
use crate::handlers::cursor::{find_symbol_at_cursor, key_at_cursor};
use crate::handlers::util::{
    location_in_dir, lookup_child_module_symbol, parent_dir, resolve_module_source,
};
use crate::handlers::{hover_attribute, hover_function, hover_module_input};

/// `textDocument/declaration` — for HCL this is identical to
/// `textDocument/definition`. Clients often call both, so we expose
/// the same behaviour under a separate name rather than returning
/// `None` for declaration requests.
pub async fn goto_declaration(
    backend: &Backend,
    params: lsp_types::request::GotoDeclarationParams,
) -> jsonrpc::Result<Option<lsp_types::request::GotoDeclarationResponse>> {
    goto_definition(backend, params).await
}

pub async fn goto_definition(
    backend: &Backend,
    params: GotoDefinitionParams,
) -> jsonrpc::Result<Option<GotoDefinitionResponse>> {
    let pos = params.text_document_position_params.position;
    let uri = params.text_document_position_params.text_document.uri;

    // Module-scoped goto-def — handled BEFORE the generic
    // `reference_at_position` path because those positions (an
    // attribute key inside a module block, the output-name segment
    // of `module.foo.bar`) are not modelled as cross-block
    // references in the symbol index. Both descend *into* the child
    // module's source, so they run first and fall through on miss.
    if let Some(doc) = backend.state.documents.get(&uri) {
        if let Some(loc) = module_input_goto_at(&backend.state, doc.value(), &uri, pos) {
            return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
        }
        if let Some(loc) = module_output_goto_at(&backend.state, doc.value(), &uri, pos) {
            return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
        }
    }

    let key = match backend.state.documents.get(&uri) {
        Some(doc) => reference_at_position(&doc, pos).map(|r| reference_key(&r.kind)),
        None => None,
    };
    let Some(key) = key else {
        return Ok(None);
    };

    // `definitions_by_name` is a workspace-wide index — a
    // `variable "region" { }` declared in the stack root, in every
    // child module, and in every unrelated stack in the workspace
    // all land under the same `(Variable, "region")` key. Scope the
    // result to the reference's own module (its parent directory):
    // Terraform's module boundary is a directory, so only those
    // declarations are visible from the reference. Cross-module
    // / cross-stack leaks are what produces the "goto-def shows
    // every `region` in the workspace" symptom.
    let reference_dir = parent_dir(&uri);
    let locations: Vec<Location> = match backend.state.definitions_by_name.get(&key) {
        Some(entry) => {
            match reference_dir.as_deref() {
                Some(dir) => {
                    let scoped: Vec<Location> = entry
                        .iter()
                        .filter(|loc| location_in_dir(loc, dir))
                        .map(|l| l.to_lsp_location())
                        .collect();
                    // If nothing in the reference's own module
                    // matches, the reference is genuinely
                    // unresolved (the undefined-reference
                    // diagnostic will flag it). Don't fall back to
                    // cross-module matches — that's the bug.
                    scoped
                }
                // Pathological URI (no parseable parent directory).
                // Be lenient and return every match so the user
                // isn't stuck on an unnavigable file — same
                // fallback `is_defined_in_module` uses.
                None => entry.iter().map(|l| l.to_lsp_location()).collect(),
            }
        }
        None => Vec::new(),
    };

    if locations.is_empty() {
        Ok(None)
    } else {
        // LSP accepts Array for any count; avoid the single-item Scalar case
        // to keep the handler branch-free.
        Ok(Some(GotoDefinitionResponse::Array(locations)))
    }
}

/// Goto-def target for a cursor sitting on an attribute key inside a
/// `module "LABEL" { KEY = … }` block. Returns the LSP location of
/// the `variable "KEY" { }` declaration in the child module — or
/// `None` if the cursor isn't on such a key, if the child module's
/// source can't be resolved to a directory, or if the child module
/// doesn't declare a variable with that name.
fn module_input_goto_at(
    state: &tfls_state::StateStore,
    doc: &DocumentState,
    uri: &Url,
    pos: Position,
) -> Option<Location> {
    let body = doc.parsed.body.as_ref()?;
    let offset = lsp_position_to_byte_offset(&doc.rope, pos).ok()?;

    let module_block = hover_module_input::find_module_block_at(body, offset)?;
    let module_label = module_block
        .labels
        .first()
        .and_then(hover_module_input::label_str)?;
    let source = hover_module_input::string_attribute(module_block, "source")?;

    // Cursor must be on an attribute key inside the module body.
    let (attr_name, _range) =
        hover_module_input::attribute_key_at(&module_block.body, offset, &doc.rope)?;
    // Don't intercept the `source` / `version` / `providers` /
    // `count` / `for_each` / `depends_on` keys themselves — those
    // aren't user-declared inputs of the child module.
    if matches!(
        attr_name.as_str(),
        "source" | "version" | "providers" | "count" | "for_each" | "depends_on"
    ) {
        return None;
    }

    let parent = parent_dir(uri)?;
    let child = resolve_module_source(&parent, &module_label, &source)?;

    lookup_child_module_symbol(state, &child, SymbolKind::Variable, &attr_name)
}

/// Goto-def target for a cursor on the output-name segment of a
/// `module.LABEL.OUTPUT` (or deeper) expression — e.g. cursor on
/// `subnet_id` in `module.network.subnet_id`. Returns the LSP
/// location of the `output "OUTPUT" { }` declaration inside the
/// child module.
///
/// Deliberately does NOT fire when the cursor is on the `module`
/// keyword or on the `LABEL` segment — those positions continue to
/// go through the generic `ReferenceKind::Module { name }` path,
/// which jumps to the `module "LABEL" { }` call-site header. The
/// mental model: navigating *on the module name* takes you to the
/// call declaration; navigating *on a value inside the module*
/// takes you to the child's source.
fn module_output_goto_at(
    state: &tfls_state::StateStore,
    doc: &DocumentState,
    uri: &Url,
    pos: Position,
) -> Option<Location> {
    let body = doc.parsed.body.as_ref()?;
    let offset = lsp_position_to_byte_offset(&doc.rope, pos).ok()?;

    let (module_label, output_name) = find_module_output_segment_at(body, offset)?;
    let source = doc.symbols.module_sources.get(&module_label)?.clone();
    let parent = parent_dir(uri)?;
    let child = resolve_module_source(&parent, &module_label, &source)?;

    lookup_child_module_symbol(state, &child, SymbolKind::Output, &output_name)
}

/// Walk the body for a `module.LABEL.OUTPUT` traversal whose OUTPUT
/// segment contains `offset`. Returns `(LABEL, OUTPUT)` on hit.
/// Descends into every nested expression shape that can contain
/// another expression, mirroring
/// `tfls_parser::references::visit_expression`.
fn find_module_output_segment_at(body: &Body, offset: usize) -> Option<(String, String)> {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if let Some(hit) = find_module_output_segment_in_expr(&attr.value, offset) {
                return Some(hit);
            }
        } else if let Some(block) = structure.as_block() {
            if let Some(hit) = find_module_output_segment_at(&block.body, offset) {
                return Some(hit);
            }
        }
    }
    None
}

/// Given the operator list of a `module.…` traversal, decide whether
/// the cursor at `offset` is on an output-name segment (or a
/// drill-in segment after it) and return `(LABEL, OUTPUT)` on hit.
///
/// Accepted shapes:
///
/// - `module.LABEL.OUTPUT`
/// - `module.LABEL.OUTPUT.field…`       (drill-in into object output)
/// - `module.LABEL.OUTPUT[n]`, `…[n].f` (drill-in through indexing)
/// - `module.LABEL[key].OUTPUT…`        (for-each / count module)
/// - `module.LABEL[key].OUTPUT[n].f…`   (for-each module + list output)
///
/// Cursor on the LABEL segment (or on anything inside an index
/// expression between the label and the output) deliberately does
/// NOT match — those positions route to the generic
/// `ReferenceKind::Module { name }` path so the user lands on the
/// `module "LABEL" { }` call-site header.
fn module_output_hit(
    operators: &[hcl_edit::repr::Decorated<TraversalOperator>],
    offset: usize,
) -> Option<(String, String)> {
    // First operator must be `GetAttr` carrying the module label.
    let label_ident = match operators.first().map(|o| o.value()) {
        Some(TraversalOperator::GetAttr(i)) => i,
        _ => return None,
    };
    let label = label_ident.as_str().to_string();

    // Skip any `Index` operators sitting between the label and the
    // output name — `module.LABEL[each.key].OUTPUT` is idiomatic for
    // `for_each`/`count` modules, and the indexing doesn't change
    // the per-module symbol table.
    let mut i = 1;
    while i < operators.len() {
        match operators[i].value() {
            TraversalOperator::Index(_) => i += 1,
            _ => break,
        }
    }

    // The next operator (if any) is the output-name GetAttr.
    let out_ident = match operators.get(i).map(|o| o.value()) {
        Some(TraversalOperator::GetAttr(ident)) => ident,
        _ => return None,
    };
    let out_span = out_ident.span()?;
    let output = out_ident.as_str().to_string();

    // Drill-in tail: cursor may sit anywhere on subsequent GetAttr
    // segments (e.g. `output.foo.bar` where `foo` is an object
    // output). We still resolve to the output declaration — we
    // don't walk into its structure. Index operators in the tail
    // aren't themselves cursor targets (they carry sub-expressions
    // handled by the caller's descent).
    let last_end = operators[i..]
        .iter()
        .filter_map(|op| match op.value() {
            TraversalOperator::GetAttr(ident) => ident.span(),
            _ => None,
        })
        .next_back()
        .map(|s| s.end)
        .unwrap_or(out_span.end);

    if offset >= out_span.start && offset <= last_end {
        Some((label, output))
    } else {
        None
    }
}

fn find_module_output_segment_in_expr(
    expr: &Expression,
    offset: usize,
) -> Option<(String, String)> {
    match expr {
        Expression::Traversal(tv) => {
            // Check this traversal's shape first; it's only a hit if
            // the base is `module` and the cursor sits on the second
            // `GetAttr` segment.
            if let Expression::Variable(v) = &tv.expr {
                if v.as_str() == "module" {
                    if let Some(hit) = module_output_hit(&tv.operators, offset) {
                        return Some(hit);
                    }
                }
            }
            // Not a direct hit — descend into children in case a
            // nested expression matches. Index operators carry their
            // own expressions (e.g. `foo[module.x.y]`).
            if let Some(hit) = find_module_output_segment_in_expr(&tv.expr, offset) {
                return Some(hit);
            }
            for op in &tv.operators {
                if let TraversalOperator::Index(e) = op.value() {
                    if let Some(hit) = find_module_output_segment_in_expr(e, offset) {
                        return Some(hit);
                    }
                }
            }
            None
        }
        Expression::FuncCall(fc) => {
            for arg in fc.args.iter() {
                if let Some(hit) = find_module_output_segment_in_expr(arg, offset) {
                    return Some(hit);
                }
            }
            None
        }
        Expression::Conditional(c) => find_module_output_segment_in_expr(&c.cond_expr, offset)
            .or_else(|| find_module_output_segment_in_expr(&c.true_expr, offset))
            .or_else(|| find_module_output_segment_in_expr(&c.false_expr, offset)),
        Expression::BinaryOp(o) => find_module_output_segment_in_expr(&o.lhs_expr, offset)
            .or_else(|| find_module_output_segment_in_expr(&o.rhs_expr, offset)),
        Expression::UnaryOp(o) => find_module_output_segment_in_expr(&o.expr, offset),
        Expression::Parenthesis(p) => find_module_output_segment_in_expr(p.inner(), offset),
        Expression::Array(a) => {
            for e in a.iter() {
                if let Some(hit) = find_module_output_segment_in_expr(e, offset) {
                    return Some(hit);
                }
            }
            None
        }
        Expression::Object(o) => {
            for (_k, v) in o.iter() {
                if let Some(hit) = find_module_output_segment_in_expr(v.expr(), offset) {
                    return Some(hit);
                }
            }
            None
        }
        Expression::ForExpr(f) => find_module_output_segment_in_expr(&f.intro.collection_expr, offset)
            .or_else(|| find_module_output_segment_in_expr(&f.value_expr, offset)),
        _ => None,
    }
}

pub async fn references(
    backend: &Backend,
    params: ReferenceParams,
) -> jsonrpc::Result<Option<Vec<Location>>> {
    let pos = params.text_document_position.position;
    let uri = params.text_document_position.text_document.uri;

    // Key lookup — works whether the cursor is on a definition or a reference.
    let key = match backend.state.documents.get(&uri) {
        Some(doc) => key_at_cursor(&doc, pos),
        None => None,
    };
    let Some(key) = key else {
        return Ok(None);
    };

    // Mirror the scope filter `goto_definition` uses (see commit
    // `584590e`): Terraform's module boundary is a single
    // directory, so a variable named `region` declared in
    // `/stackA/` and another in `/stackB/modules/net/` are
    // NOT the same symbol — they share a `SymbolKey` in the
    // workspace-wide index but live in different scopes.
    // Filter definitions + references by the reference URI's
    // parent directory.
    let reference_dir = parent_dir(&uri);
    let mut out: Vec<Location> = Vec::new();
    if params.context.include_declaration {
        if let Some(entry) = backend.state.definitions_by_name.get(&key) {
            match reference_dir.as_deref() {
                Some(dir) => out.extend(
                    entry
                        .iter()
                        .filter(|loc| location_in_dir(loc, dir))
                        .map(|l| l.to_lsp_location()),
                ),
                // Pathological URI (no parseable parent). Be
                // lenient and return every match so the user
                // isn't stuck on an unnavigable file — same
                // fallback used by `is_defined_in_module` and
                // `goto_definition`.
                None => out.extend(entry.iter().map(|l| l.to_lsp_location())),
            }
        }
    }
    if let Some(entry) = backend.state.references_by_name.get(&key) {
        match reference_dir.as_deref() {
            Some(dir) => out.extend(
                entry
                    .iter()
                    .filter(|loc| location_in_dir(loc, dir))
                    .map(|l| l.to_lsp_location()),
            ),
            None => out.extend(entry.iter().map(|l| l.to_lsp_location())),
        }
    }

    if out.is_empty() { Ok(None) } else { Ok(Some(out)) }
}

pub async fn hover(backend: &Backend, params: HoverParams) -> jsonrpc::Result<Option<Hover>> {
    let pos = params.text_document_position_params.position;
    let uri = params.text_document_position_params.text_document.uri;

    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };

    // Prefer the narrowest match: attribute hover beats symbol hover when the
    // cursor is on an attribute key inside a resource body, because the
    // resource's symbol range contains the attribute's position too.
    if let Some(hover) = hover_attribute::attribute_hover(&backend.state, &doc, pos, &uri) {
        return Ok(Some(hover));
    }

    // Module-input hover: a key inside a `module "…" {}` block points
    // at the referenced child module's variable declaration.
    if let Some(hover) = hover_module_input::module_input_hover(&backend.state, &doc, pos) {
        return Ok(Some(hover));
    }

    // Function calls come before symbol hover: function names share their
    // span with nothing in the symbol tables, but the enclosing output /
    // resource would otherwise "win" and produce a useless hover.
    if let Some(hover) = hover_function::function_hover(&backend.state, &doc, pos) {
        return Ok(Some(hover));
    }

    // Fall back to symbol under cursor (reference OR defining block label).
    if let Some(target) = find_symbol_at_cursor(&doc, pos) {
        let detail = describe_key(&target.key);
        return Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: detail,
            }),
            range: Some(target.location.range()),
        }));
    }

    Ok(None)
}

pub(crate) fn describe_key(key: &SymbolKey) -> String {
    let kind = match key.kind {
        SymbolKind::Variable => "variable",
        SymbolKind::Local => "local",
        SymbolKind::Output => "output",
        SymbolKind::Module => "module",
        SymbolKind::Resource => "resource",
        SymbolKind::DataSource => "data source",
        SymbolKind::Provider => "provider",
        SymbolKind::TerraformBlock => "terraform block",
    };
    format!("**{kind}** `{name}`", name = key.name)
}
