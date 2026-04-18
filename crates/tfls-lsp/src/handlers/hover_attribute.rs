//! Hover for attributes inside resource / data source / provider bodies.
//!
//! When the cursor sits on an attribute key (e.g. `ami` inside `resource
//! "aws_instance" "web" { ami = "..." }`), we look the attribute up in the
//! provider schema and return its description as a hover.
//!
//! Attributes live in the HCL AST, not in the symbol tables — the reference
//! indexer only records cross-block references like `var.x`. So this handler
//! walks the parsed [`hcl_edit::structure::Body`] directly, rather than
//! re-using the symbol lookup helpers used by [`crate::handlers::cursor`].
//!
//! Nested blocks (e.g. `network_interface` inside `aws_instance`) are followed
//! by recursing into the schema's `block_types`, matching the HCL block name
//! at each level.

use hcl_edit::repr::Span;
use hcl_edit::structure::{Attribute, Body};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use std::sync::Arc;
use tfls_parser::hcl_span_to_lsp_range;
use tfls_schema::{AttributeSchema, BlockSchema, ProviderSchema};
use tfls_state::{DocumentState, StateStore};

/// Try to produce a hover for an attribute key under the cursor.
pub fn attribute_hover(
    state: &StateStore,
    doc: &DocumentState,
    pos: Position,
) -> Option<Hover> {
    let body = doc.parsed.body.as_ref()?;
    let hit = find_attribute_at(body, doc, pos)?;

    let root_schema = match hit.root_kind {
        RootBlockKind::Resource => {
            let ps = state.find_resource_schema(&hit.root_type)?;
            ps.resource_schemas.get(&hit.root_type).cloned()?
        }
        RootBlockKind::DataSource => {
            let ps = state.find_data_source_schema(&hit.root_type)?;
            ps.data_source_schemas.get(&hit.root_type).cloned()?
        }
        RootBlockKind::Provider => {
            let ps = find_provider_schema(state, &hit.root_type)?;
            ps.provider.clone()
        }
    };

    let block_schema = descend_schema(&root_schema.block, &hit.nested_path)?;
    let attr_schema = block_schema.attributes.get(&hit.attr_name)?;

    let markdown = render_attribute(&hit, attr_schema);
    let range = hcl_span_to_lsp_range(&doc.rope, hit.key_span).ok()?;

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(range),
    })
}

fn find_provider_schema(state: &StateStore, name: &str) -> Option<Arc<ProviderSchema>> {
    for entry in state.schemas.iter() {
        if entry.key().r#type == name {
            return Some(entry.value().clone());
        }
    }
    None
}

/// Root-level block flavour that owns this attribute — decides whether we
/// look the attribute up as a resource, data source, or provider attribute.
#[derive(Debug, Clone, Copy)]
enum RootBlockKind {
    Resource,
    DataSource,
    Provider,
}

struct AttributeHit {
    root_kind: RootBlockKind,
    /// The terraform type for the root block — `aws_instance` for `resource
    /// "aws_instance" "web" {}`, or the provider name for `provider "aws" {}`.
    root_type: String,
    /// Names of nested blocks between the root and the attribute's parent.
    /// Empty for top-level attributes of the root block.
    nested_path: Vec<String>,
    attr_name: String,
    key_span: std::ops::Range<usize>,
}

fn find_attribute_at(body: &Body, doc: &DocumentState, pos: Position) -> Option<AttributeHit> {
    let offset = tfls_parser::lsp_position_to_byte_offset(&doc.rope, pos).ok()?;

    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };

        let block_name = block.ident.as_str();
        let root_kind = match block_name {
            "resource" => RootBlockKind::Resource,
            "data" => RootBlockKind::DataSource,
            "provider" => RootBlockKind::Provider,
            _ => continue,
        };

        let Some(type_label) = block.labels.first().map(label_text) else {
            continue;
        };

        if !span_contains(block.span(), offset) {
            continue;
        }

        if let Some(mut hit) = scan_block(&block.body, offset, &mut Vec::new()) {
            hit.root_kind = root_kind;
            hit.root_type = type_label;
            return Some(hit);
        }
    }
    None
}

fn scan_block(body: &Body, offset: usize, nested: &mut Vec<String>) -> Option<AttributeHit> {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if attribute_key_contains(attr, offset) {
                let key_span = attr.key.span()?;
                return Some(AttributeHit {
                    // overwritten by caller in find_attribute_at
                    root_kind: RootBlockKind::Resource,
                    root_type: String::new(),
                    nested_path: nested.clone(),
                    attr_name: attr.key.as_str().to_string(),
                    key_span,
                });
            }
        } else if let Some(block) = structure.as_block() {
            if !span_contains(block.span(), offset) {
                continue;
            }
            let name = block.ident.as_str().to_string();
            nested.push(name);
            let hit = scan_block(&block.body, offset, nested);
            nested.pop();
            if let Some(h) = hit {
                return Some(h);
            }
        }
    }
    None
}

fn attribute_key_contains(attr: &Attribute, offset: usize) -> bool {
    let Some(span) = attr.key.span() else {
        return false;
    };
    offset >= span.start && offset <= span.end
}

fn span_contains(span: Option<std::ops::Range<usize>>, offset: usize) -> bool {
    matches!(span, Some(r) if offset >= r.start && offset <= r.end)
}

fn label_text(label: &hcl_edit::structure::BlockLabel) -> String {
    match label {
        hcl_edit::structure::BlockLabel::String(s) => s.value().to_string(),
        hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
    }
}

fn descend_schema<'a>(root: &'a BlockSchema, path: &[String]) -> Option<&'a BlockSchema> {
    let mut current = root;
    for name in path {
        let nested = current.block_types.get(name)?;
        current = &nested.block;
    }
    Some(current)
}

fn render_attribute(hit: &AttributeHit, schema: &AttributeSchema) -> String {
    let mut out = String::new();
    let header_kind = match hit.root_kind {
        RootBlockKind::Resource => "resource",
        RootBlockKind::DataSource => "data source",
        RootBlockKind::Provider => "provider",
    };

    out.push_str(&format!(
        "**{kind}** `{root}`",
        kind = header_kind,
        root = hit.root_type
    ));
    if !hit.nested_path.is_empty() {
        out.push_str(&format!(" / `{}`", hit.nested_path.join(".")));
    }
    out.push_str("\n\n");

    out.push_str(&format!("**attribute** `{}`", hit.attr_name));

    let mut flags = Vec::new();
    if schema.required {
        flags.push("required");
    } else if schema.optional {
        flags.push("optional");
    }
    if schema.computed {
        flags.push("computed");
    }
    if schema.sensitive {
        flags.push("sensitive");
    }
    if schema.deprecated {
        flags.push("deprecated");
    }
    if !flags.is_empty() {
        out.push_str(&format!(" _{}_", flags.join(", ")));
    }
    out.push('\n');

    if let Some(desc) = schema.description.as_deref() {
        if !desc.trim().is_empty() {
            out.push('\n');
            out.push_str(desc);
        }
    }

    out
}
