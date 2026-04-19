//! Completion handler — classifies the cursor context and returns
//! schema-derived or symbol-table-derived suggestions.
//!
//! Where appropriate, completions use LSP snippet syntax
//! (`InsertTextFormat::SNIPPET`) so the client can offer tabstop
//! navigation through placeholders.

use std::collections::{BTreeSet, HashSet};

use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, Body};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse, CompletionTextEdit,
    Documentation, InsertTextFormat, MarkupContent, MarkupKind, Position, Range, TextEdit, Url,
};
use tfls_core::{
    BlockKind, CompletionContext, IndexRootRef, META_ATTRS, PathStep, ResourceAddress,
    VariableType, classify_context, is_singleton_meta_block, merge_shapes, meta_blocks,
};
use tfls_parser::lsp_position_to_byte_offset;
use tfls_schema::NestingMode;
use tower_lsp::jsonrpc;

use super::util::{parent_dir, resolve_module_source};

use crate::backend::Backend;

/// Top-level block snippets: (label, snippet body, detail).
///
/// `resource` and `data` intentionally stop at the opening quote of
/// the type label. The per-type completion (`resource_type_items` /
/// `data_source_type_items`) then takes over once the user types a
/// character and produces the full scaffold — including required
/// attributes and a `${1:name}` placeholder for the instance name.
/// Emitting the full scaffold here as well would make the two chain
/// badly (the per-type scaffold cannot safely run inside a closed
/// placeholder without duplicating braces).
const TOP_LEVEL_SNIPPETS: &[(&str, &str, &str)] = &[
    ("resource", "resource \"", "Resource block"),
    ("data", "data \"", "Data source block"),
    (
        "variable",
        "variable \"${1:name}\" {\n  type = ${2:string}\n  $0\n}",
        "Variable block",
    ),
    (
        "output",
        "output \"${1:name}\" {\n  value = $2\n  $0\n}",
        "Output block",
    ),
    (
        "module",
        "module \"${1:name}\" {\n  source = \"${2:source}\"\n  $0\n}",
        "Module block",
    ),
    (
        "provider",
        "provider \"${1:name}\" {\n  $0\n}",
        "Provider block",
    ),
    ("terraform", "terraform {\n  $0\n}", "Terraform block"),
    ("locals", "locals {\n  $0\n}", "Locals block"),
];

pub async fn completion(
    backend: &Backend,
    params: CompletionParams,
) -> jsonrpc::Result<Option<CompletionResponse>> {
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;

    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };

    let offset = match lsp_position_to_byte_offset(&doc.rope, pos) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "completion: failed to map cursor to byte offset");
            return Ok(None);
        }
    };
    let text = doc.rope.to_string();
    let ctx = classify_context(&text, offset);

    let label_closed = label_closed_after(&text, offset);
    let items = match ctx {
        CompletionContext::TopLevel => top_level_items(),
        CompletionContext::ResourceType => {
            if label_closed {
                resource_type_items_bare(backend)
            } else {
                resource_type_items(backend)
            }
        }
        CompletionContext::DataSourceType => {
            if label_closed {
                data_source_type_items_bare(backend)
            } else {
                data_source_type_items(backend)
            }
        }
        CompletionContext::ResourceBody { resource_type } => {
            let body = doc.parsed.body.as_ref();
            let filter = compute_body_filter(body, offset);
            let nested_path = body
                .map(|b| nested_block_path(b, offset))
                .unwrap_or_default();
            resource_body_items(backend, &resource_type, /*data=*/ false, &filter, &nested_path)
        }
        CompletionContext::DataSourceBody { resource_type } => {
            let body = doc.parsed.body.as_ref();
            let filter = compute_body_filter(body, offset);
            let nested_path = body
                .map(|b| nested_block_path(b, offset))
                .unwrap_or_default();
            resource_body_items(backend, &resource_type, /*data=*/ true, &filter, &nested_path)
        }
        CompletionContext::VariableRef => {
            module_symbol_items(backend, &uri, SymbolField::Variables, CompletionItemKind::VARIABLE)
        }
        CompletionContext::LocalRef => {
            module_symbol_items(backend, &uri, SymbolField::Locals, CompletionItemKind::VARIABLE)
        }
        CompletionContext::ModuleRef => {
            module_symbol_items(backend, &uri, SymbolField::Modules, CompletionItemKind::MODULE)
        }
        CompletionContext::VariableAttrRef { path } => variable_attr_items(backend, &uri, &path),
        CompletionContext::ResourceRef { resource_type } => {
            resource_name_items(backend, &uri, &resource_type, /*data=*/ false)
        }
        CompletionContext::ResourceAttr { resource_type, .. } => {
            resource_attr_items(backend, &resource_type, /*data=*/ false)
        }
        CompletionContext::DataSourceRef { resource_type } => {
            resource_name_items(backend, &uri, &resource_type, /*data=*/ true)
        }
        CompletionContext::DataSourceAttr { resource_type, .. } => {
            resource_attr_items(backend, &resource_type, /*data=*/ true)
        }
        CompletionContext::IndexKeyRef { root, path } => {
            let line = doc.rope.line(pos.line as usize).to_string();
            index_key_items(backend, &uri, &root, &path, &line, pos)
        }
        CompletionContext::ModuleBody { name } => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            module_input_items(backend, &uri, &name, &filter)
        }
        CompletionContext::ModuleAttr { module_name } => {
            module_output_items(backend, &uri, &module_name)
        }
        CompletionContext::AttributeValue {
            resource_type,
            attr_name,
        } => attribute_value_items(backend, &resource_type, &attr_name),
        CompletionContext::FunctionCall => function_name_items(backend),
        CompletionContext::Unknown => Vec::new(),
    };

    if items.is_empty() {
        Ok(None)
    } else {
        Ok(Some(CompletionResponse::Array(items)))
    }
}

fn top_level_items() -> Vec<CompletionItem> {
    TOP_LEVEL_SNIPPETS
        .iter()
        .map(|(label, snippet, detail)| CompletionItem {
            label: (*label).to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some((*detail).to_string()),
            insert_text: Some((*snippet).to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect()
}

fn resource_type_items(backend: &Backend) -> Vec<CompletionItem> {
    backend
        .state
        .all_resource_types()
        .into_iter()
        .map(|name| {
            let snippet = resource_scaffold_snippet(&name, backend, "resource");
            CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::CLASS),
                detail: Some("resource type".to_string()),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect()
}

fn data_source_type_items(backend: &Backend) -> Vec<CompletionItem> {
    backend
        .state
        .all_data_source_types()
        .into_iter()
        .map(|name| {
            let snippet = resource_scaffold_snippet(&name, backend, "data");
            CompletionItem {
                label: name,
                kind: Some(CompletionItemKind::CLASS),
                detail: Some("data source type".to_string()),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect()
}

/// Bare-name variants used when the first label is already closed —
/// e.g. the cursor sits inside an active `${1:type}` placeholder from
/// an outer `resource`/`data` snippet. Emitting the full scaffold in
/// that case duplicates the outer snippet's closing quote + name label
/// + body and produces malformed code.
fn resource_type_items_bare(backend: &Backend) -> Vec<CompletionItem> {
    backend
        .state
        .all_resource_types()
        .into_iter()
        .map(|name| CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some("resource type".to_string()),
            insert_text: Some(name),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect()
}

fn data_source_type_items_bare(backend: &Backend) -> Vec<CompletionItem> {
    backend
        .state
        .all_data_source_types()
        .into_iter()
        .map(|name| CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some("data source type".to_string()),
            insert_text: Some(name),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect()
}

/// Whether the cursor sits inside an already-closed quoted label: walk
/// from the cursor to end of line, skip identifier chars, and check if
/// the next char is `"`. Used by the handler to choose scaffold vs
/// bare-name completion items.
fn label_closed_after(text: &str, offset: usize) -> bool {
    if offset > text.len() {
        return false;
    }
    let after = &text[offset..];
    let end = after.find('\n').unwrap_or(after.len());
    let tail = &after[..end];
    let rest = tail.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_');
    rest.starts_with('"')
}

/// Build a snippet that completes the type name and scaffolds the block
/// with required attributes as tabstops.
///
/// The completion triggers inside `resource "` so the type name is
/// already preceded by `resource "`. The snippet closes the first quote,
/// adds the instance name, braces, and required attrs:
///
/// ```text
/// aws_instance" "${1:name}" {
///   ami           = "${2}"
///   instance_type = "${3}"
///   $0
/// }
/// ```
pub fn resource_scaffold_snippet(type_name: &str, backend: &Backend, kind: &str) -> String {
    let schema = if kind == "data" {
        backend.state.data_source_schema(type_name)
    } else {
        backend.state.resource_schema(type_name)
    };

    let mut snippet = format!("{type_name}\" \"${{1:name}}\" {{\n");
    let mut tab = 2;
    let mut had_required = false;

    if let Some(schema) = schema {
        let mut required: Vec<(&String, &tfls_schema::AttributeSchema)> = schema
            .block
            .attributes
            .iter()
            .filter(|(_, a)| a.required)
            .collect();
        required.sort_by_key(|(name, _)| name.as_str());
        had_required = !required.is_empty();
        for (name, _) in &required {
            snippet.push_str(&format!("  {name} = \"${{{tab}}}\"\n"));
            tab += 1;
        }
    }

    // When there are required attrs, end the block right after the last
    // one — the user tabs through the required values and exits the
    // snippet cleanly. When there are none, leave an empty body line
    // with `$0` so the cursor lands inside the block for free-form
    // editing.
    if !had_required {
        snippet.push_str("  $0\n");
    }
    snippet.push('}');
    snippet
}

fn resource_body_items(
    backend: &Backend,
    type_name: &str,
    data: bool,
    filter: &BodyFilter,
    nested_path: &[String],
) -> Vec<CompletionItem> {
    let schema = if data {
        backend.state.data_source_schema(type_name)
    } else {
        backend.state.resource_schema(type_name)
    };
    let schema = match schema {
        Some(s) => s,
        None => return Vec::new(),
    };

    // Descend through nested block types to land on the schema that
    // actually governs the cursor's surrounding block. An unknown
    // nested name (user typo, provider mismatch) yields no suggestions
    // rather than leaking the outer resource's attributes.
    let mut block_schema = &schema.block;
    for step in nested_path {
        match block_schema.block_types.get(step) {
            Some(nb) => block_schema = &nb.block,
            None => return Vec::new(),
        }
    }

    let mut items: Vec<CompletionItem> = block_schema
        .attributes
        .iter()
        .filter(|(name, attr)| {
            // Skip pure-computed attributes — they're provider outputs
            // the user can't assign. `optional && computed` stays in:
            // the user can still set those, the provider just has a
            // fallback. `required` implies writable regardless of
            // `computed`.
            !filter.present_attrs.contains(name.as_str())
                && (attr.required || attr.optional)
        })
        .map(|(name, attr)| CompletionItem {
            label: name.clone(),
            kind: Some(if attr.required {
                CompletionItemKind::FIELD
            } else {
                CompletionItemKind::PROPERTY
            }),
            detail: Some(attribute_detail(attr)),
            documentation: attr.description.as_ref().map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d.clone(),
                })
            }),
            insert_text: Some(format!("{name} = ${{1}}")),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect();

    items.extend(
        block_schema
            .block_types
            .iter()
            .filter(|(name, nb)| {
                // Suggest repeatable nested blocks even when one is
                // already present; skip only schema-`single` blocks
                // that have already been placed.
                nb.nesting_mode != NestingMode::Single
                    || !filter.present_blocks.contains(name.as_str())
            })
            .map(|(name, _)| CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some("nested block".to_string()),
                insert_text: Some(format!("{name} {{\n  $0\n}}")),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }),
    );

    // Meta-arguments (`count`, `for_each`, `lifecycle`, …) are valid
    // only at the top level of a resource or data body — not inside
    // nested schema blocks like `root_block_device` or `lifecycle`.
    if nested_path.is_empty() {
        let kind = if data { BlockKind::Data } else { BlockKind::Resource };
        items.extend(
            meta_argument_items(kind)
                .into_iter()
                .filter(|item| !filter.present_attrs.contains(&item.label)),
        );
        items.extend(meta_block_items(kind).into_iter().filter(|item| {
            !(is_singleton_meta_block(kind, &item.label)
                && filter.present_blocks.contains(&item.label))
        }));
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Attributes and nested blocks already present in the enclosing
/// block at the completion cursor, used to suppress duplicate
/// suggestions.
#[derive(Default)]
struct BodyFilter {
    present_attrs: HashSet<String>,
    present_blocks: HashSet<String>,
}

fn compute_body_filter(body_opt: Option<&Body>, offset: usize) -> BodyFilter {
    let Some(body) = body_opt else {
        return BodyFilter::default();
    };
    let Some(block) = innermost_block_at(body, offset) else {
        return BodyFilter::default();
    };
    let mut out = BodyFilter::default();
    for structure in block.body.iter() {
        if let Some(attr) = structure.as_attribute() {
            out.present_attrs.insert(attr.key.as_str().to_string());
        } else if let Some(nested) = structure.as_block() {
            out.present_blocks.insert(nested.ident.as_str().to_string());
        }
    }
    out
}

fn innermost_block_at(body: &Body, offset: usize) -> Option<&Block> {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if !span_contains_offset(block.span(), offset) {
            continue;
        }
        return Some(innermost_block_at(&block.body, offset).unwrap_or(block));
    }
    None
}

/// Nested-block names from the outer `resource`/`data` body down to
/// (but not including) the innermost block containing `offset`.
/// Returns an empty `Vec` when the cursor is directly in the
/// resource/data body — which is the common case and the current
/// behaviour of `resource_body_items` before this change.
fn nested_block_path(body: &Body, offset: usize) -> Vec<String> {
    let mut path = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        // Skip the outer `resource`/`data`/`module`/etc. block header —
        // the caller already knows the root type from the
        // `CompletionContext`. We only record nested-block names below
        // it.
        if span_contains_offset(block.span(), offset) {
            collect_nested_path(&block.body, offset, &mut path);
            return path;
        }
    }
    path
}

fn collect_nested_path(body: &Body, offset: usize, path: &mut Vec<String>) {
    for structure in body.iter() {
        let Some(nested) = structure.as_block() else {
            continue;
        };
        if !span_contains_offset(nested.span(), offset) {
            continue;
        }
        path.push(nested.ident.as_str().to_string());
        collect_nested_path(&nested.body, offset, path);
        return;
    }
}

fn span_contains_offset(span: Option<std::ops::Range<usize>>, offset: usize) -> bool {
    matches!(span, Some(r) if offset >= r.start && offset <= r.end)
}

fn meta_argument_items(_kind: BlockKind) -> Vec<CompletionItem> {
    META_ATTRS
        .iter()
        .map(|name| {
            // `depends_on` takes a list; others take a scalar or ref.
            let snippet = if *name == "depends_on" {
                format!("{name} = [${{1}}]")
            } else {
                format!("{name} = ${{1}}")
            };
            CompletionItem {
                label: (*name).to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("meta-argument".to_string()),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect()
}

fn meta_block_items(kind: BlockKind) -> Vec<CompletionItem> {
    meta_blocks(kind)
        .iter()
        .map(|name| {
            // `provisioner` takes a type label; others are plain blocks.
            let snippet = if *name == "provisioner" {
                "provisioner \"${1:local-exec}\" {\n  $0\n}".to_string()
            } else {
                format!("{name} {{\n  $0\n}}")
            };
            CompletionItem {
                label: (*name).to_string(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some("meta-block".to_string()),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect()
}

fn attribute_detail(attr: &tfls_schema::AttributeSchema) -> String {
    let mut parts = Vec::new();
    if attr.required {
        parts.push("required");
    }
    if attr.optional {
        parts.push("optional");
    }
    if attr.computed {
        parts.push("computed");
    }
    if attr.sensitive {
        parts.push("sensitive");
    }
    if attr.deprecated {
        parts.push("deprecated");
    }
    if parts.is_empty() {
        "attribute".to_string()
    } else {
        parts.join(", ")
    }
}

fn function_name_items(backend: &Backend) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = backend
        .state
        .functions
        .iter()
        .map(|entry| {
            let name = entry.key().clone();
            let sig = entry.value();
            CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(sig.label(&name)),
                documentation: sig.description.as_ref().map(|d| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: d.clone(),
                    })
                }),
                insert_text: Some(format!("{name}(${{1}})$0")),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Context-aware value completions: suggest matching resources, data
/// sources, variables, locals, and functions based on the attribute
/// being edited.
fn attribute_value_items(
    backend: &Backend,
    resource_type: &str,
    attr_name: &str,
) -> Vec<CompletionItem> {
    use std::collections::HashSet;
    use super::attr_ref_map;

    let mut items = Vec::new();
    let mut sort_index = 0u32;

    let known_types: HashSet<String> = backend.state.all_resource_types().into_iter().collect();

    // If we know what resource type this attribute references, suggest
    // matching resources and data sources first.
    if let Some(target_type) =
        attr_ref_map::referenced_resource_type(resource_type, attr_name, &known_types)
    {
        let out_attr = attr_ref_map::output_attribute(attr_name);

        // Resources of the target type.
        for name in backend.state.resources_of_type(&target_type) {
            let ref_expr = format!("{target_type}.{name}{out_attr}");
            items.push(CompletionItem {
                label: ref_expr.clone(),
                kind: Some(CompletionItemKind::REFERENCE),
                detail: Some(format!("resource {target_type}")),
                sort_text: Some(format!("{sort_index:04}_{}", ref_expr)),
                ..Default::default()
            });
            sort_index += 1;
        }

        // Data sources of the target type.
        for name in backend.state.data_sources_of_type(&target_type) {
            let ref_expr = format!("data.{target_type}.{name}{out_attr}");
            items.push(CompletionItem {
                label: ref_expr.clone(),
                kind: Some(CompletionItemKind::REFERENCE),
                detail: Some(format!("data source {target_type}")),
                sort_text: Some(format!("{sort_index:04}_{}", ref_expr)),
                ..Default::default()
            });
            sort_index += 1;
        }
    }

    // Always suggest variables and locals — we don't know their types
    // but the user may have named them appropriately.
    for name in backend.state.all_variable_names() {
        let ref_expr = format!("var.{name}");
        items.push(CompletionItem {
            label: ref_expr.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some("variable".to_string()),
            sort_text: Some(format!("{sort_index:04}_{}", ref_expr)),
            ..Default::default()
        });
        sort_index += 1;
    }

    for name in backend.state.all_local_names() {
        let ref_expr = format!("local.{name}");
        items.push(CompletionItem {
            label: ref_expr.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some("local".to_string()),
            sort_text: Some(format!("{sort_index:04}_{}", ref_expr)),
            ..Default::default()
        });
        sort_index += 1;
    }

    // Also include functions for cases like `coalesce(var.x, "default")`.
    items.extend(function_name_items(backend));

    items
}

fn symbol_name_items<'a, I: IntoIterator<Item = &'a String>>(
    names: I,
    kind: CompletionItemKind,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = names
        .into_iter()
        .map(|name| CompletionItem {
            label: name.clone(),
            kind: Some(kind),
            ..Default::default()
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Which named-symbol bucket of a module to draw from.
#[derive(Debug, Clone, Copy)]
enum SymbolField {
    Variables,
    Locals,
    Modules,
}

/// Gather a sorted, de-duplicated list of names from every `.tf` file
/// in the same module (directory) as `uri`.
fn module_symbol_items(
    backend: &Backend,
    uri: &Url,
    field: SymbolField,
    kind: CompletionItemKind,
) -> Vec<CompletionItem> {
    let dir = parent_dir(uri);
    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), dir.as_deref()) {
            continue;
        }
        let doc = entry.value();
        match field {
            SymbolField::Variables => {
                for n in doc.symbols.variables.keys() {
                    names.insert(n.clone());
                }
            }
            SymbolField::Locals => {
                for n in doc.symbols.locals.keys() {
                    names.insert(n.clone());
                }
            }
            SymbolField::Modules => {
                for n in doc.symbols.modules.keys() {
                    names.insert(n.clone());
                }
            }
        }
    }
    symbol_name_items(names.iter(), kind)
}

/// Names of declared resources (or data sources) of `type_name` across
/// the current module.
fn resource_name_items(
    backend: &Backend,
    uri: &Url,
    type_name: &str,
    data: bool,
) -> Vec<CompletionItem> {
    let dir = parent_dir(uri);
    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), dir.as_deref()) {
            continue;
        }
        let table = &entry.value().symbols;
        let addrs: Box<dyn Iterator<Item = &ResourceAddress>> = if data {
            Box::new(table.data_sources.keys())
        } else {
            Box::new(table.resources.keys())
        };
        for addr in addrs {
            if addr.resource_type == type_name {
                names.insert(addr.name.clone());
            }
        }
    }
    symbol_name_items(names.iter(), CompletionItemKind::FIELD)
}

/// Attributes available on a resource/data source via the provider
/// schema (e.g. `aws_iam_role.role1.|` → `arn`, `id`, `name`, …).
fn resource_attr_items(backend: &Backend, type_name: &str, data: bool) -> Vec<CompletionItem> {
    let schema = if data {
        backend.state.data_source_schema(type_name)
    } else {
        backend.state.resource_schema(type_name)
    };
    let Some(schema) = schema else {
        return Vec::new();
    };
    let mut items: Vec<CompletionItem> = schema
        .block
        .attributes
        .iter()
        .map(|(name, attr)| CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(attribute_detail(attr)),
            documentation: attr.description.as_ref().map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d.clone(),
                })
            }),
            insert_text: Some(name.clone()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Drill into a variable's `type = object({ … })` declaration along
/// `path` (where `path[0]` is the variable name and subsequent entries
/// are nested field names). Returns the keys of the resolved object as
/// completion items; anything not resolving to an object yields empty.
fn variable_attr_items(backend: &Backend, uri: &Url, path: &[String]) -> Vec<CompletionItem> {
    let (var_name, rest) = match path.split_first() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let dir = parent_dir(uri);
    let mut ty: Option<VariableType> = None;
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), dir.as_deref()) {
            continue;
        }
        if let Some(t) = entry.value().symbols.variable_types.get(var_name) {
            ty = Some(t.clone());
            break;
        }
    }
    let Some(mut current) = ty else {
        return Vec::new();
    };
    for segment in rest {
        current = match current {
            VariableType::Object(mut fields) => match fields.remove(segment.as_str()) {
                Some(next) => next,
                None => return Vec::new(),
            },
            _ => return Vec::new(),
        };
    }
    let VariableType::Object(fields) = current else {
        return Vec::new();
    };
    let mut items: Vec<CompletionItem> = fields
        .iter()
        .map(|(name, sub)| CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(format!("{sub}")),
            insert_text: Some(name.clone()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

fn doc_in_dir(doc_uri: &Url, dir: Option<&std::path::Path>) -> bool {
    match dir {
        // Without a resolvable parent dir for the active doc, don't
        // over-filter — include everything.
        None => true,
        Some(d) => parent_dir(doc_uri).as_deref() == Some(d),
    }
}

/// Resolve the child module directory referenced by `module_name`
/// in the active document's `module_sources`. Returns `None` if the
/// module isn't declared, the source is missing, or the source can't
/// be resolved (no matching local path, no lockfile entry).
fn resolve_child_module_dir(
    backend: &Backend,
    uri: &Url,
    module_name: &str,
) -> Option<std::path::PathBuf> {
    let parent = parent_dir(uri)?;
    let doc = backend.state.documents.get(uri)?;
    let source = doc.symbols.module_sources.get(module_name)?.clone();
    resolve_module_source(&parent, module_name, &source)
}

/// Input-variable completions inside `module "NAME" { | }`.
fn module_input_items(
    backend: &Backend,
    uri: &Url,
    module_name: &str,
    filter: &BodyFilter,
) -> Vec<CompletionItem> {
    let Some(child_dir) = resolve_child_module_dir(backend, uri, module_name) else {
        return Vec::new();
    };
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), Some(child_dir.as_path())) {
            continue;
        }
        let table = &entry.value().symbols;
        for (name, sym) in &table.variables {
            if filter.present_attrs.contains(name) || !seen.insert(name.clone()) {
                continue;
            }
            let type_detail = table.variable_types.get(name).map(|t| format!("{t}"));
            let documentation = sym.doc.clone().map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d,
                })
            });
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: type_detail,
                documentation,
                insert_text: Some(format!("{name} = ${{1}}")),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Output completions for `module.NAME.|`.
fn module_output_items(
    backend: &Backend,
    uri: &Url,
    module_name: &str,
) -> Vec<CompletionItem> {
    let Some(child_dir) = resolve_child_module_dir(backend, uri, module_name) else {
        return Vec::new();
    };
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), Some(child_dir.as_path())) {
            continue;
        }
        for (name, sym) in &entry.value().symbols.outputs {
            if !seen.insert(name.clone()) {
                continue;
            }
            let documentation = sym.doc.clone().map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d,
                })
            });
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some("module output".to_string()),
                documentation,
                insert_text: Some(name.clone()),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                ..Default::default()
            });
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Resolve the statically-known shape of the reference rooted at
/// `root`, merged across peer documents in the active module.
fn shape_for_root(
    backend: &Backend,
    uri: &Url,
    root: &IndexRootRef,
) -> Option<VariableType> {
    let dir = parent_dir(uri);
    let mut acc: Option<VariableType> = None;
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), dir.as_deref()) {
            continue;
        }
        let table = &entry.value().symbols;
        let next: Option<VariableType> = match root {
            IndexRootRef::Variable { name } => {
                let ty = table.variable_types.get(name).cloned();
                let default = table.variable_defaults.get(name).cloned();
                match (ty, default) {
                    (Some(a), Some(b)) => Some(merge_shapes(a, b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                }
            }
            IndexRootRef::Local { name } => table.local_shapes.get(name).cloned(),
            IndexRootRef::Resource {
                resource_type,
                name,
            } => table
                .for_each_shapes
                .get(&ResourceAddress::new(resource_type.clone(), name.clone()))
                .cloned(),
            IndexRootRef::DataSource {
                resource_type,
                name,
            } => table
                .data_source_for_each_shapes
                .get(&ResourceAddress::new(resource_type.clone(), name.clone()))
                .cloned(),
            IndexRootRef::Module { module_name } => {
                table.module_for_each_shapes.get(module_name).cloned()
            }
        };
        if let Some(found) = next {
            acc = Some(match acc {
                Some(prev) => merge_shapes(prev, found),
                None => found,
            });
        }
    }
    acc
}

/// Walk `shape` along `path`, returning the sub-shape at the end.
fn walk_shape(shape: &VariableType, path: &[PathStep]) -> Option<VariableType> {
    let mut current = shape.clone();
    for step in path {
        current = match (current, step) {
            (VariableType::Object(fields), PathStep::Bracket(k))
            | (VariableType::Object(fields), PathStep::Attr(k)) => {
                fields.get(k).cloned()?
            }
            (VariableType::Map(value), PathStep::Bracket(_)) => *value,
            (VariableType::Tuple(items), PathStep::Bracket(k)) => {
                let idx: usize = k.parse().ok()?;
                items.get(idx).cloned()?
            }
            (VariableType::List(value), PathStep::Bracket(_))
            | (VariableType::Set(value), PathStep::Bracket(_)) => *value,
            _ => return None,
        };
    }
    Some(current)
}

fn index_key_items(
    backend: &Backend,
    uri: &Url,
    root: &IndexRootRef,
    path: &[PathStep],
    line: &str,
    pos: Position,
) -> Vec<CompletionItem> {
    let Some(root_shape) = shape_for_root(backend, uri, root) else {
        return Vec::new();
    };
    let Some(current) = walk_shape(&root_shape, path) else {
        return Vec::new();
    };
    let VariableType::Object(fields) = current else {
        return Vec::new();
    };

    let replace_range = compute_index_replace_range(line, pos);

    let mut items: Vec<CompletionItem> = fields
        .iter()
        .map(|(name, sub)| CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format!("{sub}")),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range,
                // Always include the closing `]`; the replace range
                // consumes any existing trailing `]` so we never end
                // up with duplicates.
                new_text: format!("\"{name}\"]"),
            })),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Compute the text-edit replace range for a bracket-index completion.
///
/// Starts just after the most recent `[` on the cursor's line and
/// extends forward over any partial key the user already typed
/// (identifier chars, hyphens, or quotes) plus a trailing `]` if one
/// is on the same line. The emitted text always includes its own `]`,
/// so an existing `]` is replaced rather than duplicated.
fn compute_index_replace_range(line: &str, pos: Position) -> Range {
    let col = pos.character as usize;
    let before = line.get(..col).unwrap_or("");
    let bracket_col = before.rfind('[').map(|b| b + 1).unwrap_or(col);

    let after = &line[col..];
    let mut consumed = 0usize;
    let mut done = false;
    for c in after.chars() {
        if done {
            break;
        }
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' | '"' => {
                consumed += c.len_utf8();
            }
            ']' => {
                consumed += 1;
                done = true;
            }
            _ => break,
        }
    }

    Range {
        start: Position::new(pos.line, bracket_col as u32),
        end: Position::new(pos.line, (col + consumed) as u32),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod compute_index_replace_range_tests {
    use super::*;

    fn range(line: &str, col: u32) -> (u32, u32) {
        let r = compute_index_replace_range(line, Position::new(0, col));
        (r.start.character, r.end.character)
    }

    #[test]
    fn cursor_right_after_bracket_no_close() {
        // `aws_vpc.eu[|` (column 11, `[` at column 10).
        let (s, e) = range("aws_vpc.eu[", 11);
        assert_eq!((s, e), (11, 11));
    }

    #[test]
    fn cursor_inside_brackets_with_close_present() {
        // `aws_vpc.eu[|xxx]` — consume `xxx]`.
        let (s, e) = range("aws_vpc.eu[xxx]", 11);
        assert_eq!((s, e), (11, 15));
    }

    #[test]
    fn cursor_after_open_quote() {
        // `aws_vpc.eu["|partial"]`
        let (s, e) = range("aws_vpc.eu[\"partial\"]", 12);
        // start is at `"` (after `[`), end consumes everything through `]`.
        assert_eq!((s, e), (11, 21));
    }

    #[test]
    fn cursor_between_quotes_empty_key() {
        // `aws_vpc.eu["|"]` — consume `"` + `]`.
        let (s, e) = range("aws_vpc.eu[\"\"]", 12);
        assert_eq!((s, e), (11, 14));
    }

    #[test]
    fn cursor_after_empty_brackets() {
        // `aws_vpc.eu[|]` — consume just `]`.
        let (s, e) = range("aws_vpc.eu[]", 11);
        assert_eq!((s, e), (11, 12));
    }
}

