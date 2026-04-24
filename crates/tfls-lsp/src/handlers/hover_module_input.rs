//! Hover on an attribute key inside a `module "…" { … }` block.
//!
//! Resolves the module's `source = "…"` to a child directory (either a
//! local path or a `.terraform/modules/modules.json`-cached one), then
//! looks the attribute name up among the child module's `variable`
//! declarations and renders the variable's `description` and `type`
//! as markdown.

use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, Body};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use tfls_parser::{hcl_span_to_lsp_range, lsp_position_to_byte_offset};
use tfls_state::{DocumentState, StateStore};

use super::util::{parent_dir, resolve_module_source};

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
