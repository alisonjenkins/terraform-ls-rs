//! Hover on anything module-related.
//!
//! Three distinct cursor positions are handled here, each with its own
//! detector + renderer:
//!
//! 1. **Input attribute key** inside a `module "…" { KEY = … }` block —
//!    shows that key's variable description + type from the child
//!    module.
//! 2. **Output reference attribute** in `module.X.OUTPUT` — shows that
//!    output's description from the child module.
//! 3. **Module name** (either the `"X"` label on a `module "X" {}`
//!    block or the `X` segment in a `module.X…` reference) — renders
//!    a module-overview markdown with every input + output listed.

use hcl_edit::expr::{Expression, TraversalOperator};
use hcl_edit::repr::{Decorated, Span};
use hcl_edit::structure::{Block, Body};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Range};
use tfls_parser::{hcl_span_to_lsp_range, lsp_position_to_byte_offset};
use tfls_state::{DocumentState, StateStore};

use super::util::{module_source_in_dir, parent_dir, resolve_module_source};

pub fn module_input_hover(
    state: &StateStore,
    doc: &DocumentState,
    pos: Position,
) -> Option<Hover> {
    let body = doc.parsed.body.as_ref()?;
    let offset = lsp_position_to_byte_offset(&doc.rope, pos).ok()?;

    // Walk the top-level body to find a module block whose body
    // contains the cursor offset.
    let module_block = find_module_block_at(body, offset)?;
    let module_label = module_block.labels.first().and_then(label_str)?;
    let source = string_attribute(module_block, "source")?;

    // Cursor must be on an attribute key inside the module body.
    let (attr_name, key_range) = attribute_key_at(&module_block.body, offset, &doc.rope)?;

    // Resolve the source to a child dir and find the matching variable.
    let dir = parent_dir(&doc.uri)?;
    let child = resolve_module_source(&dir, &module_label, &source)?;

    let (var_symbol, var_type) = child_variable(state, &child, &attr_name)?;
    let markdown = render(&attr_name, &var_type, var_symbol.doc.as_deref());

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(key_range),
    })
}

pub(crate) fn label_str(label: &hcl_edit::structure::BlockLabel) -> Option<String> {
    Some(match label {
        hcl_edit::structure::BlockLabel::String(s) => s.value().to_string(),
        hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
    })
}

pub(crate) fn string_attribute(block: &Block, key: &str) -> Option<String> {
    for structure in block.body.iter() {
        let Some(attr) = structure.as_attribute() else {
            continue;
        };
        if attr.key.as_str() != key {
            continue;
        }
        if let hcl_edit::expr::Expression::String(s) = &attr.value {
            return Some(s.value().to_string());
        }
    }
    None
}

pub(crate) fn find_module_block_at(body: &Body, offset: usize) -> Option<&Block> {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "module" {
            continue;
        }
        if span_contains(block.span(), offset) {
            return Some(block);
        }
    }
    None
}

pub(crate) fn attribute_key_at(
    body: &Body,
    offset: usize,
    rope: &ropey::Rope,
) -> Option<(String, lsp_types::Range)> {
    for structure in body.iter() {
        let Some(attr) = structure.as_attribute() else {
            continue;
        };
        let key_span = attr.key.span()?;
        if offset >= key_span.start && offset <= key_span.end {
            let range = hcl_span_to_lsp_range(rope, key_span).ok()?;
            return Some((attr.key.as_str().to_string(), range));
        }
    }
    None
}

fn child_variable(
    state: &StateStore,
    child_dir: &std::path::Path,
    attr_name: &str,
) -> Option<(
    tfls_core::Symbol,
    tfls_core::VariableType,
)> {
    for entry in state.documents.iter() {
        let Ok(doc_path) = entry.key().to_file_path() else {
            continue;
        };
        if doc_path.parent() != Some(child_dir) {
            continue;
        }
        let table = &entry.value().symbols;
        if let Some(sym) = table.variables.get(attr_name) {
            let ty = table
                .variable_types
                .get(attr_name)
                .cloned()
                .unwrap_or(tfls_core::VariableType::Any);
            return Some((sym.clone(), ty));
        }
    }
    None
}

fn render(name: &str, ty: &tfls_core::VariableType, description: Option<&str>) -> String {
    let mut out = format!("### `{name}`  *({ty})*");
    if let Some(d) = description {
        if !d.is_empty() {
            out.push_str("\n\n");
            out.push_str(d);
        }
    }
    out
}

fn span_contains(span: Option<std::ops::Range<usize>>, offset: usize) -> bool {
    matches!(span, Some(r) if offset >= r.start && offset <= r.end)
}

/// Hover on the OUTPUT segment of a `module.X.OUTPUT` (or deeper)
/// reference: renders the child module's `output "OUTPUT" { }`
/// declaration as markdown with the `description` attribute inlined.
///
/// Terraform doesn't statically type outputs — we show the name and
/// description only. The cursor position rules match
/// `module_output_goto_at`: label → not us, OUTPUT or drill-in tail →
/// us.
pub fn module_output_ref_hover(
    state: &StateStore,
    doc: &DocumentState,
    uri: &lsp_types::Url,
    pos: Position,
) -> Option<Hover> {
    let body = doc.parsed.body.as_ref()?;
    let offset = lsp_position_to_byte_offset(&doc.rope, pos).ok()?;

    let hit = find_module_output_segment_for_hover(body, offset, &doc.rope)?;

    let parent = parent_dir(uri)?;
    let source = doc
        .symbols
        .module_sources
        .get(&hit.label)
        .cloned()
        .or_else(|| module_source_in_dir(state, &parent, &hit.label))?;
    let child = resolve_module_source(&parent, &hit.label, &source)?;
    let (output_sym, output_description) = child_output(state, &child, &hit.output)?;
    let _ = output_sym;

    let markdown = render_output_ref(&hit.label, &hit.output, output_description.as_deref());
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(hit.range),
    })
}

/// Hover on a module name: either the `"X"` label of a `module "X" {}`
/// block header, or the `X` segment in a `module.X…` reference.
/// Renders an overview of the child module: source, plus every input
/// (name, type, required / default, description) and every output
/// (name, description).
pub fn module_overview_hover(
    state: &StateStore,
    doc: &DocumentState,
    uri: &lsp_types::Url,
    pos: Position,
) -> Option<Hover> {
    let body = doc.parsed.body.as_ref()?;
    let offset = lsp_position_to_byte_offset(&doc.rope, pos).ok()?;

    // Prefer the block-header label when the cursor sits on it — the
    // `module "X" {}` author is most often looking for a reminder of
    // the child's inputs.
    let hit = find_module_name_at(body, offset, &doc.rope)?;

    let parent = parent_dir(uri)?;
    let source = doc
        .symbols
        .module_sources
        .get(&hit.label)
        .cloned()
        .or_else(|| module_source_in_dir(state, &parent, &hit.label))?;
    let child = resolve_module_source(&parent, &hit.label, &source)?;

    let overview = build_module_overview(state, &child, &hit.label, &source);
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: overview,
        }),
        range: Some(hit.range),
    })
}

struct OutputRefHit {
    label: String,
    output: String,
    range: Range,
}

fn find_module_output_segment_for_hover(
    body: &Body,
    offset: usize,
    rope: &ropey::Rope,
) -> Option<OutputRefHit> {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if let Some(hit) = scan_expr_for_output_ref(&attr.value, offset, rope) {
                return Some(hit);
            }
        } else if let Some(block) = structure.as_block() {
            if let Some(hit) = find_module_output_segment_for_hover(&block.body, offset, rope) {
                return Some(hit);
            }
        }
    }
    None
}

fn scan_expr_for_output_ref(
    expr: &Expression,
    offset: usize,
    rope: &ropey::Rope,
) -> Option<OutputRefHit> {
    match expr {
        Expression::Traversal(tv) => {
            if let Expression::Variable(v) = &tv.expr {
                if v.as_str() == "module" {
                    if let Some(hit) = module_output_hit_for_hover(&tv.operators, offset, rope) {
                        return Some(hit);
                    }
                }
            }
            if let Some(hit) = scan_expr_for_output_ref(&tv.expr, offset, rope) {
                return Some(hit);
            }
            for op in &tv.operators {
                if let TraversalOperator::Index(e) = op.value() {
                    if let Some(hit) = scan_expr_for_output_ref(e, offset, rope) {
                        return Some(hit);
                    }
                }
            }
            None
        }
        Expression::FuncCall(fc) => fc
            .args
            .iter()
            .find_map(|arg| scan_expr_for_output_ref(arg, offset, rope)),
        Expression::Conditional(c) => scan_expr_for_output_ref(&c.cond_expr, offset, rope)
            .or_else(|| scan_expr_for_output_ref(&c.true_expr, offset, rope))
            .or_else(|| scan_expr_for_output_ref(&c.false_expr, offset, rope)),
        Expression::BinaryOp(o) => scan_expr_for_output_ref(&o.lhs_expr, offset, rope)
            .or_else(|| scan_expr_for_output_ref(&o.rhs_expr, offset, rope)),
        Expression::UnaryOp(o) => scan_expr_for_output_ref(&o.expr, offset, rope),
        Expression::Parenthesis(p) => scan_expr_for_output_ref(p.inner(), offset, rope),
        Expression::Array(a) => a
            .iter()
            .find_map(|e| scan_expr_for_output_ref(e, offset, rope)),
        Expression::Object(obj) => obj
            .iter()
            .find_map(|(_k, v)| scan_expr_for_output_ref(v.expr(), offset, rope)),
        Expression::ForExpr(f) => scan_expr_for_output_ref(&f.intro.collection_expr, offset, rope)
            .or_else(|| {
                f.key_expr
                    .as_ref()
                    .and_then(|k| scan_expr_for_output_ref(k, offset, rope))
            })
            .or_else(|| scan_expr_for_output_ref(&f.value_expr, offset, rope))
            .or_else(|| {
                f.cond
                    .as_ref()
                    .and_then(|c| scan_expr_for_output_ref(&c.expr, offset, rope))
            }),
        _ => None,
    }
}

fn module_output_hit_for_hover(
    operators: &[Decorated<TraversalOperator>],
    offset: usize,
    rope: &ropey::Rope,
) -> Option<OutputRefHit> {
    let label_ident = match operators.first().map(|o| o.value()) {
        Some(TraversalOperator::GetAttr(i)) => i,
        _ => return None,
    };
    let label = label_ident.as_str().to_string();

    let mut i = 1;
    while i < operators.len() {
        match operators[i].value() {
            TraversalOperator::Index(_) => i += 1,
            _ => break,
        }
    }

    let out_ident = match operators.get(i).map(|o| o.value()) {
        Some(TraversalOperator::GetAttr(ident)) => ident,
        _ => return None,
    };
    let out_span = out_ident.span()?;
    if offset < out_span.start || offset > out_span.end {
        return None;
    }
    let range = hcl_span_to_lsp_range(rope, out_span).ok()?;
    Some(OutputRefHit {
        label,
        output: out_ident.as_str().to_string(),
        range,
    })
}

fn child_output(
    state: &StateStore,
    child_dir: &std::path::Path,
    output_name: &str,
) -> Option<(tfls_core::Symbol, Option<String>)> {
    for entry in state.documents.iter() {
        let Ok(doc_path) = entry.key().to_file_path() else {
            continue;
        };
        if doc_path.parent() != Some(child_dir) {
            continue;
        }
        let table = &entry.value().symbols;
        if let Some(sym) = table.outputs.get(output_name) {
            let desc = sym.doc.clone();
            return Some((sym.clone(), desc));
        }
    }
    None
}

fn render_output_ref(module_label: &str, output_name: &str, description: Option<&str>) -> String {
    let mut out = format!("### `module.{module_label}.{output_name}`");
    if let Some(d) = description {
        if !d.is_empty() {
            out.push_str("\n\n");
            out.push_str(d);
        }
    }
    out
}

struct ModuleNameHit {
    label: String,
    range: Range,
}

fn find_module_name_at(body: &Body, offset: usize, rope: &ropey::Rope) -> Option<ModuleNameHit> {
    for structure in body.iter() {
        if let Some(block) = structure.as_block() {
            // Block-header label: `module "X" {}` — cursor on `"X"`.
            if block.ident.as_str() == "module" {
                if let Some(label) = block.labels.first() {
                    if let Some(span) = label.span() {
                        if offset >= span.start && offset <= span.end {
                            let text = label_str(label)?;
                            let range = hcl_span_to_lsp_range(rope, span).ok()?;
                            return Some(ModuleNameHit { label: text, range });
                        }
                    }
                }
            }
            if let Some(hit) = find_module_name_at(&block.body, offset, rope) {
                return Some(hit);
            }
        } else if let Some(attr) = structure.as_attribute() {
            if let Some(hit) = scan_expr_for_module_label(&attr.value, offset, rope) {
                return Some(hit);
            }
        }
    }
    None
}

fn scan_expr_for_module_label(
    expr: &Expression,
    offset: usize,
    rope: &ropey::Rope,
) -> Option<ModuleNameHit> {
    match expr {
        Expression::Traversal(tv) => {
            if let Expression::Variable(v) = &tv.expr {
                if v.as_str() == "module" {
                    // First operator after `module` is the label.
                    if let Some(op) = tv.operators.first() {
                        if let TraversalOperator::GetAttr(ident) = op.value() {
                            if let Some(span) = ident.span() {
                                if offset >= span.start && offset <= span.end {
                                    let range = hcl_span_to_lsp_range(rope, span).ok()?;
                                    return Some(ModuleNameHit {
                                        label: ident.as_str().to_string(),
                                        range,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            if let Some(hit) = scan_expr_for_module_label(&tv.expr, offset, rope) {
                return Some(hit);
            }
            for op in &tv.operators {
                if let TraversalOperator::Index(e) = op.value() {
                    if let Some(hit) = scan_expr_for_module_label(e, offset, rope) {
                        return Some(hit);
                    }
                }
            }
            None
        }
        Expression::FuncCall(fc) => fc
            .args
            .iter()
            .find_map(|arg| scan_expr_for_module_label(arg, offset, rope)),
        Expression::Conditional(c) => scan_expr_for_module_label(&c.cond_expr, offset, rope)
            .or_else(|| scan_expr_for_module_label(&c.true_expr, offset, rope))
            .or_else(|| scan_expr_for_module_label(&c.false_expr, offset, rope)),
        Expression::BinaryOp(o) => scan_expr_for_module_label(&o.lhs_expr, offset, rope)
            .or_else(|| scan_expr_for_module_label(&o.rhs_expr, offset, rope)),
        Expression::UnaryOp(o) => scan_expr_for_module_label(&o.expr, offset, rope),
        Expression::Parenthesis(p) => scan_expr_for_module_label(p.inner(), offset, rope),
        Expression::Array(a) => a
            .iter()
            .find_map(|e| scan_expr_for_module_label(e, offset, rope)),
        Expression::Object(obj) => obj
            .iter()
            .find_map(|(_k, v)| scan_expr_for_module_label(v.expr(), offset, rope)),
        Expression::ForExpr(f) => scan_expr_for_module_label(&f.intro.collection_expr, offset, rope)
            .or_else(|| {
                f.key_expr
                    .as_ref()
                    .and_then(|k| scan_expr_for_module_label(k, offset, rope))
            })
            .or_else(|| scan_expr_for_module_label(&f.value_expr, offset, rope))
            .or_else(|| {
                f.cond
                    .as_ref()
                    .and_then(|c| scan_expr_for_module_label(&c.expr, offset, rope))
            }),
        _ => None,
    }
}

fn build_module_overview(
    state: &StateStore,
    child_dir: &std::path::Path,
    module_label: &str,
    source: &str,
) -> String {
    let mut inputs: Vec<ModuleInput> = Vec::new();
    let mut outputs: Vec<ModuleOutput> = Vec::new();
    for entry in state.documents.iter() {
        let Ok(doc_path) = entry.key().to_file_path() else {
            continue;
        };
        if doc_path.parent() != Some(child_dir) {
            continue;
        }
        let table = &entry.value().symbols;
        for (name, sym) in &table.variables {
            let ty = table
                .variable_types
                .get(name)
                .cloned()
                .unwrap_or(tfls_core::VariableType::Any);
            inputs.push(ModuleInput {
                name: name.clone(),
                ty,
                description: sym.doc.clone(),
                required: !table.variable_defaults.contains_key(name),
            });
        }
        for (name, sym) in &table.outputs {
            outputs.push(ModuleOutput {
                name: name.clone(),
                description: sym.doc.clone(),
            });
        }
    }
    inputs.sort_by(|a, b| a.name.cmp(&b.name));
    outputs.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = format!("### `module.{module_label}`\n\n**Source:** `{source}`\n");
    if inputs.is_empty() {
        out.push_str("\n_No inputs declared._\n");
    } else {
        out.push_str("\n#### Inputs\n\n");
        for input in &inputs {
            let required_tag = if input.required { ", required" } else { "" };
            out.push_str(&format!(
                "- `{}` *({}{required_tag})*",
                input.name, input.ty
            ));
            if let Some(desc) = input.description.as_deref() {
                if !desc.is_empty() {
                    out.push_str(" — ");
                    out.push_str(desc);
                }
            }
            out.push('\n');
        }
    }
    if outputs.is_empty() {
        out.push_str("\n_No outputs declared._\n");
    } else {
        out.push_str("\n#### Outputs\n\n");
        for output in &outputs {
            out.push_str(&format!("- `{}`", output.name));
            if let Some(desc) = output.description.as_deref() {
                if !desc.is_empty() {
                    out.push_str(" — ");
                    out.push_str(desc);
                }
            }
            out.push('\n');
        }
    }
    out
}

struct ModuleInput {
    name: String,
    ty: tfls_core::VariableType,
    description: Option<String>,
    required: bool,
}

struct ModuleOutput {
    name: String,
    description: Option<String>,
}
