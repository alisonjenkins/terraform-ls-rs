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
use crate::handlers::util::{lookup_child_module_symbol, parent_dir, resolve_module_source};
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

    let locations: Vec<Location> = backend
        .state
        .definitions_by_name
        .get(&key)
        .map(|entry| entry.iter().map(|l| l.to_lsp_location()).collect())
        .unwrap_or_default();

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
                    // Collect the full trailing `.ident.ident...` prefix
                    // so drill-in expressions like `module.foo.bar.baz`
                    // (where `bar` is an object output and `baz` is one
                    // of its fields) still resolve to `output "bar" { }`.
                    // Structural drill-down into object output fields is
                    // deliberately out of scope — we stop at the output
                    // declaration.
                    let mut segments: Vec<(String, std::ops::Range<usize>)> = Vec::new();
                    for op in &tv.operators {
                        if let TraversalOperator::GetAttr(ident) = op.value() {
                            let Some(span) = ident.span() else {
                                break;
                            };
                            segments.push((ident.as_str().to_string(), span));
                        } else {
                            break;
                        }
                    }
                    if segments.len() >= 2 {
                        let (label, _) = segments[0].clone();
                        let (output, out_span) = segments[1].clone();
                        let last_end = segments
                            .last()
                            .map(|(_, s)| s.end)
                            .unwrap_or(out_span.end);
                        // Match when the cursor is anywhere from the
                        // output-name segment through the tail of the
                        // traversal. Cursor on `module` or the LABEL
                        // segment falls through — those positions go
                        // to the `module "LABEL" { }` call header via
                        // the generic reference path.
                        if offset >= out_span.start && offset <= last_end {
                            return Some((label, output));
                        }
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

    let mut out: Vec<Location> = Vec::new();
    if params.context.include_declaration {
        if let Some(entry) = backend.state.definitions_by_name.get(&key) {
            out.extend(entry.iter().map(|l| l.to_lsp_location()));
        }
    }
    if let Some(entry) = backend.state.references_by_name.get(&key) {
        out.extend(entry.iter().map(|l| l.to_lsp_location()));
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
