//! Completion handler — classifies the cursor context and returns
//! schema-derived or symbol-table-derived suggestions.
//!
//! Where appropriate, completions use LSP snippet syntax
//! (`InsertTextFormat::SNIPPET`) so the client can offer tabstop
//! navigation through placeholders.

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse, Documentation,
    InsertTextFormat, MarkupContent, MarkupKind,
};
use tfls_core::{CompletionContext, classify_context};
use tfls_parser::lsp_position_to_byte_offset;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

/// Top-level block snippets: (label, snippet body, detail).
const TOP_LEVEL_SNIPPETS: &[(&str, &str, &str)] = &[
    (
        "resource",
        "resource \"${1:type}\" \"${2:name}\" {\n  $0\n}",
        "Resource block",
    ),
    (
        "data",
        "data \"${1:type}\" \"${2:name}\" {\n  $0\n}",
        "Data source block",
    ),
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

    let items = match ctx {
        CompletionContext::TopLevel => top_level_items(),
        CompletionContext::ResourceType => resource_type_items(backend),
        CompletionContext::DataSourceType => data_source_type_items(backend),
        CompletionContext::ResourceBody { resource_type } => {
            resource_body_items(backend, &resource_type, /*data=*/ false)
        }
        CompletionContext::DataSourceBody { resource_type } => {
            resource_body_items(backend, &resource_type, /*data=*/ true)
        }
        CompletionContext::VariableRef => {
            symbol_name_items(doc.symbols.variables.keys(), CompletionItemKind::VARIABLE)
        }
        CompletionContext::LocalRef => {
            symbol_name_items(doc.symbols.locals.keys(), CompletionItemKind::VARIABLE)
        }
        CompletionContext::ModuleRef => {
            symbol_name_items(doc.symbols.modules.keys(), CompletionItemKind::MODULE)
        }
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
fn resource_scaffold_snippet(type_name: &str, backend: &Backend, kind: &str) -> String {
    let schema = if kind == "data" {
        backend.state.data_source_schema(type_name)
    } else {
        backend.state.resource_schema(type_name)
    };

    let mut snippet = format!("{type_name}\" \"${{1:name}}\" {{\n");
    let mut tab = 2;

    if let Some(schema) = schema {
        let mut required: Vec<(&String, &tfls_schema::AttributeSchema)> = schema
            .block
            .attributes
            .iter()
            .filter(|(_, a)| a.required)
            .collect();
        required.sort_by_key(|(name, _)| name.as_str());
        for (name, _) in &required {
            snippet.push_str(&format!("  {name} = \"${{{tab}}}\"\n"));
            tab += 1;
        }
    }

    snippet.push_str("  $0\n}");
    snippet
}

fn resource_body_items(backend: &Backend, type_name: &str, data: bool) -> Vec<CompletionItem> {
    let schema = if data {
        backend.state.data_source_schema(type_name)
    } else {
        backend.state.resource_schema(type_name)
    };
    let schema = match schema {
        Some(s) => s,
        None => return Vec::new(),
    };

    let mut items: Vec<CompletionItem> = schema
        .block
        .attributes
        .iter()
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

    items.extend(schema.block.block_types.keys().map(|name| CompletionItem {
        label: name.clone(),
        kind: Some(CompletionItemKind::STRUCT),
        detail: Some("nested block".to_string()),
        insert_text: Some(format!("{name} {{\n  $0\n}}")),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    }));

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
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
