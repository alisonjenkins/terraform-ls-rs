//! Completion handler — classifies the cursor context and returns
//! schema-derived or symbol-table-derived suggestions.
//!
//! Where appropriate, completions use LSP snippet syntax
//! (`InsertTextFormat::SNIPPET`) so the client can offer tabstop
//! navigation through placeholders.

use std::collections::{BTreeSet, HashMap, HashSet};

use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, Body};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemTag, CompletionParams, CompletionResponse,
    CompletionTextEdit, Documentation, InsertTextFormat, MarkupContent, MarkupKind, Position,
    Range, TextEdit,
};
use tfls_core::{
    builtin_blocks, classify_context, is_singleton_meta_block, merge_shapes, meta_blocks,
    BlockKind, CompletionContext, IndexRootRef, PathStep, ResourceAddress, VariableType,
    META_ATTRS,
};
use tfls_parser::lsp_position_to_byte_offset;
use tfls_schema::NestingMode;
use tower_lsp_server::jsonrpc;
use url::Url;

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
        "variable \"${1:name}\" {\n  default = ${2}\n  description = \"${3}\"\n  type = ${4:string}\n}",
        "Variable block",
    ),
    (
        "output",
        "output \"${1:name}\" {\n  description = \"${2}\"\n  value = ${3}\n}",
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
    (
        "import",
        "import {\n  to = ${1}\n  id = \"${2}\"\n}",
        "Import block (Terraform 1.5+)",
    ),
    (
        "moved",
        "moved {\n  from = ${1}\n  to   = ${2}\n}",
        "Moved block (Terraform 1.1+)",
    ),
    (
        "removed",
        "removed {\n  from = ${1}\n  lifecycle {\n    destroy = ${2:false}\n  }\n}",
        "Removed block (Terraform 1.7+)",
    ),
    (
        "check",
        "check \"${1:name}\" {\n  assert {\n    condition     = ${2}\n    error_message = \"${3}\"\n  }\n}",
        "Check block (Terraform 1.5+)",
    ),
];

pub async fn completion(
    backend: &Backend,
    params: CompletionParams,
) -> jsonrpc::Result<Option<CompletionResponse>> {
    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document_position.text_document.uri)
    else {
        return Ok(None);
    };
    let pos = params.text_document_position.position;
    let trigger_char = params
        .context
        .as_ref()
        .and_then(|c| c.trigger_character.as_deref());

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

    // A `:` trigger is only useful as the second `:` of `provider::`
    // (provider-defined function namespace). It also fires on every
    // ternary / for-expression / object-literal colon — bail cheaply on
    // those before any materialization or classify. The byte before the
    // typed `:` must itself be `:`.
    if trigger_char == Some(":") {
        let prev_is_colon = offset
            .checked_sub(2)
            .is_some_and(|i| doc.rope.byte(i) == b':');
        if !prev_is_colon {
            return Ok(None);
        }
    }
    // classify_context only reads `&text[..offset]` and label_closed_after
    // only reads up to the next newline after the cursor — so materialize
    // just through the end of the cursor's line, not the whole document.
    // On a large file with the cursor anywhere but the last line this
    // avoids stringifying the entire post-cursor body each keystroke.
    let line_idx = pos.line as usize;
    let line_end = if line_idx + 1 < doc.rope.len_lines() {
        doc.rope.line_to_byte(line_idx + 1)
    } else {
        doc.rope.len_bytes()
    };

    // `.tfvars` files are not HCL config — their top level is a flat set
    // of `variable_name = value` assignments. The block-oriented
    // classifier would mislabel a partial key as TopLevel and offer
    // `resource`/`variable`/… block snippets, which are invalid here.
    // Route them to a dedicated key/value completion instead.
    if is_tfvars_uri(&uri) {
        let cur_line_start = doc.rope.line_to_byte(line_idx);
        let line_prefix = doc.rope.byte_slice(cur_line_start..offset).to_string();
        let items = tfvars_completion_items(backend, &uri, doc.parsed.body.as_ref(), &line_prefix);
        return Ok(Some(CompletionResponse::Array(items)));
    }

    // Test files (`.tftest.hcl` / `.tftest.json`) have their own closed
    // grammar (`run`, `mock_provider`, `override_*`, …) and reference the
    // module under test. The block-oriented `.tf` classifier would offer
    // `resource` / `data` here and miss `run` entirely, so route them to a
    // dedicated completion path (mirrors the `.tfvars` early-return).
    if tfls_core::uri::is_tftest_uri(uri.as_str()) {
        let cur_line_start = doc.rope.line_to_byte(line_idx);
        let line_prefix = doc.rope.byte_slice(cur_line_start..offset).to_string();
        let items = tftest_completion_items(
            backend,
            &uri,
            doc.parsed.body.as_ref(),
            &line_prefix,
            offset,
        );
        return Ok(Some(CompletionResponse::Array(items)));
    }

    let text = doc.rope.byte_slice(..line_end).to_string();
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
            // A cursor directly on a `dynamic "<label>" { … }` body
            // (NOT inside its `content { }` child) should offer the
            // three dynamic meta-args + a `content` scaffold, not
            // the target block's attrs — those belong inside
            // `content`. Detect by asking for the innermost enclosing
            // block; if it's literally `dynamic`, route to the
            // dedicated menu.
            if cursor_on_dynamic_body(body, offset) {
                dynamic_body_items(&filter)
            } else {
                let nested_path = body
                    .map(|b| nested_block_path(b, offset))
                    .unwrap_or_default();
                resource_body_items(
                    backend,
                    &resource_type,
                    /*data=*/ false,
                    &filter,
                    &nested_path,
                )
            }
        }
        CompletionContext::DataSourceBody { resource_type } => {
            let body = doc.parsed.body.as_ref();
            let filter = compute_body_filter(body, offset);
            if cursor_on_dynamic_body(body, offset) {
                dynamic_body_items(&filter)
            } else {
                let nested_path = body
                    .map(|b| nested_block_path(b, offset))
                    .unwrap_or_default();
                resource_body_items(
                    backend,
                    &resource_type,
                    /*data=*/ true,
                    &filter,
                    &nested_path,
                )
            }
        }
        CompletionContext::VariableRef => module_symbol_items(
            backend,
            &uri,
            SymbolField::Variables,
            CompletionItemKind::VARIABLE,
        ),
        CompletionContext::LocalRef => module_symbol_items(
            backend,
            &uri,
            SymbolField::Locals,
            CompletionItemKind::VARIABLE,
        ),
        CompletionContext::ModuleRef => module_symbol_items(
            backend,
            &uri,
            SymbolField::Modules,
            CompletionItemKind::MODULE,
        ),
        CompletionContext::EachRef => each_namespace_items(),
        CompletionContext::CountRef => count_namespace_items(),
        CompletionContext::PathRef => path_namespace_items(),
        CompletionContext::TerraformNamespaceRef => terraform_namespace_items(),
        CompletionContext::SelfRef { resource_type } => {
            // `self` resolves to the enclosing resource — same
            // attribute set as `<resource_type>.<name>.` would
            // produce. Reuse the resource_attr_items helper.
            resource_attr_items(backend, &resource_type, /*data=*/ false)
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
        CompletionContext::FunctionCall => {
            // Lead with for/ternary comprehension scaffolds (sorted on top
            // via `00_`), then the alphabetical function list.
            let mut items = expression_scaffold_items();
            items.extend(function_name_items(backend));
            items
        }
        CompletionContext::ExpressionRoot => expression_root_items(backend, &uri),
        CompletionContext::IgnoreChangesList { resource_type } => {
            ignore_changes_items(backend, &resource_type)
        }
        CompletionContext::DependsOnList => depends_on_address_items(backend, &uri),
        CompletionContext::EachAttr { path } => {
            each_attr_items(backend, &uri, doc.parsed.body.as_ref(), offset, &path)
        }
        CompletionContext::ForBindingRef { root, path } => {
            drill_collection_element_fields(backend, &uri, &root, &path)
        }
        CompletionContext::ProviderFunctionNamespace => {
            provider_function_namespace_items(backend, &uri)
        }
        CompletionContext::ProviderFunctionName { provider_local } => {
            provider_function_name_items(backend, &uri, &provider_local)
        }
        CompletionContext::TerraformBlockBody => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            builtin_body_items(builtin_blocks::TERRAFORM_BLOCK, &filter)
        }
        CompletionContext::VariableBlockBody { .. } => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            builtin_body_items(builtin_blocks::VARIABLE_BLOCK, &filter)
        }
        CompletionContext::OutputBlockBody { .. } => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            builtin_body_items(builtin_blocks::OUTPUT_BLOCK, &filter)
        }
        CompletionContext::LocalsBlockBody => Vec::new(),
        CompletionContext::ImportBlockBody => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            builtin_body_items(builtin_blocks::IMPORT_BLOCK, &filter)
        }
        CompletionContext::MovedBlockBody => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            builtin_body_items(builtin_blocks::MOVED_BLOCK, &filter)
        }
        CompletionContext::RemovedBlockBody => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            builtin_body_items(builtin_blocks::REMOVED_BLOCK, &filter)
        }
        CompletionContext::CheckBlockBody => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            builtin_body_items(builtin_blocks::CHECK_BLOCK, &filter)
        }
        CompletionContext::ProviderBlockBody { name } => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            provider_block_body_items(backend, &name, &filter)
        }
        CompletionContext::BackendBlockBody { name } => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            match builtin_blocks::backend_schema(&name) {
                Some(schema) => builtin_body_items(schema, &filter),
                None => Vec::new(),
            }
        }
        CompletionContext::RequiredProvidersBody => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            required_providers_entry_items(&filter)
        }
        CompletionContext::RequiredProvidersEntryBody => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            required_provider_entry_attr_items(&filter)
        }
        CompletionContext::RequiredProviderSourceValue => {
            required_provider_source_value_items().await
        }
        CompletionContext::RequiredProviderVersionValue {
            source,
            cursor_partial,
        } => {
            let items = required_provider_version_value_items(
                &backend.http_client,
                source.as_deref(),
                &cursor_partial,
            )
            .await;
            stamp_version_replace(items, pos, &cursor_partial)
        }
        CompletionContext::RequiredVersionValue { cursor_partial } => {
            let items = required_version_value_items(&cursor_partial).await;
            stamp_version_replace(items, pos, &cursor_partial)
        }
        CompletionContext::VariableTypeValue => variable_type_value_items(),
        CompletionContext::ModuleVersionValue {
            source,
            cursor_partial,
        } => {
            let items = module_version_value_items(
                &backend.http_client,
                source.as_deref(),
                &cursor_partial,
            )
            .await;
            stamp_version_replace(items, pos, &cursor_partial)
        }
        CompletionContext::BuiltinNestedBody { path } => {
            let filter = compute_body_filter(doc.parsed.body.as_ref(), offset);
            match tfls_core::resolve_nested_schema(&path) {
                Some(schema) => builtin_body_items(schema, &filter),
                None => Vec::new(),
            }
        }
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
/// Snippet for a nested-block completion — `name { … }` with each
/// required attr pre-filled on its own line and given a type-aware
/// placeholder. Tabstops are numbered so the user can Tab through
/// required values in order; when there are no required attrs the
/// body is a single `$0` so the cursor lands inside the block.
fn nested_block_scaffold_snippet(name: &str, block: &tfls_schema::BlockSchema) -> String {
    let mut required: Vec<(&String, &tfls_schema::AttributeSchema)> = block
        .attributes
        .iter()
        .filter(|(_, a)| a.required)
        .collect();
    required.sort_by_key(|(n, _)| n.as_str());

    if required.is_empty() {
        return format!("{name} {{\n  $0\n}}");
    }

    let mut out = format!("{name} {{\n");
    for (i, (attr_name, attr)) in required.iter().enumerate() {
        let tab = i + 1;
        let placeholder = match classify_schema_type(attr.r#type.as_ref()) {
            SchemaTypeKind::String => format!("\"${{{tab}}}\""),
            SchemaTypeKind::Sequence => format!("[${{{tab}}}]"),
            SchemaTypeKind::Mapping => format!("{{\n    ${{{tab}}}\n  }}"),
            SchemaTypeKind::Scalar => format!("${{{tab}}}"),
        };
        out.push_str(&format!("  {attr_name} = {placeholder}\n"));
    }
    out.push('}');
    out
}

pub fn resource_scaffold_snippet(type_name: &str, backend: &Backend, kind: &str) -> String {
    // Borrow the Schema out of the provider's Arc rather than deep-cloning
    // it via resource_schema(): the type-menu builds a scaffold for every
    // one of ~1000-1400 types per keystroke, and a full Schema clone each
    // time is the dominant cost. find_*_schema returns an Arc (one cheap
    // refcount bump) we can read the required attrs from in place.
    let provider = if kind == "data" {
        backend.state.find_data_source_schema(type_name)
    } else {
        backend.state.find_resource_schema(type_name)
    };
    let schema = provider.as_ref().and_then(|p| {
        if kind == "data" {
            p.data_source_schemas.get(type_name)
        } else {
            p.resource_schemas.get(type_name)
        }
    });

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
        for (name, attr) in &required {
            // Type-aware placeholder so a required number/bool/list/object
            // doesn't insert a quoted string and immediately error
            // (matches nested_block_scaffold_snippet).
            let placeholder = match classify_schema_type(attr.r#type.as_ref()) {
                SchemaTypeKind::String => format!("\"${{{tab}}}\""),
                SchemaTypeKind::Sequence => format!("[${{{tab}}}]"),
                SchemaTypeKind::Mapping => format!("{{\n    ${{{tab}}}\n  }}"),
                SchemaTypeKind::Scalar => format!("${{{tab}}}"),
            };
            snippet.push_str(&format!("  {name} = {placeholder}\n"));
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

/// Terraform type-expression completion: primitives as plain values
/// and collection constructors as snippets with a tabstop on the
/// inner type so the user can nest immediately. The detector
/// populates `VariableTypeValue` both for `type = |` and for
/// positions recursively inside constructors (`list(|)`,
/// `object({ name = | })`, …), so the same item list is the right
/// answer in every position.
fn variable_type_value_items() -> Vec<CompletionItem> {
    const PRIMITIVES: &[(&str, &str)] = &[
        ("string", "A Unicode string value"),
        ("number", "A numeric value (integer or float)"),
        ("bool", "A boolean: `true` or `false`"),
        (
            "any",
            "Accept any type. Typically used for pass-through variables",
        ),
        ("null", "The null value"),
    ];
    const CONSTRUCTORS: &[(&str, &str, &str)] = &[
        (
            "list",
            "list(${1:string})",
            "Ordered sequence of values of the same type",
        ),
        (
            "set",
            "set(${1:string})",
            "Unordered collection of unique values",
        ),
        (
            "map",
            "map(${1:string})",
            "Key-value mapping. Keys are strings, values share the given type",
        ),
        (
            "tuple",
            "tuple([${1:string}])",
            "Ordered sequence where each position has its own type",
        ),
        (
            "object",
            "object({\n  ${1:name} = ${2:string}\n})",
            "Record where each named attribute has its own type",
        ),
    ];
    let mut items: Vec<CompletionItem> = Vec::new();
    for (prim, doc) in PRIMITIVES {
        items.push(CompletionItem {
            label: prim.to_string(),
            kind: Some(CompletionItemKind::TYPE_PARAMETER),
            detail: Some((*doc).to_string()),
            insert_text: Some(prim.to_string()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            // `00_` prefix keeps primitives on top of constructors.
            sort_text: Some(format!("00_{prim}")),
            ..Default::default()
        });
    }
    for (label, snippet, doc) in CONSTRUCTORS {
        items.push(CompletionItem {
            label: label.to_string(),
            kind: Some(CompletionItemKind::CONSTRUCTOR),
            detail: Some((*doc).to_string()),
            insert_text: Some((*snippet).to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            sort_text: Some(format!("01_{label}")),
            ..Default::default()
        });
    }
    // `optional` is only meaningful inside an `object({…})` attribute, so it
    // trails the structural constructors (`02_` prefix). Omitted optional
    // attributes resolve to `null` unless a default is supplied.
    items.push(CompletionItem {
        label: "optional".to_string(),
        kind: Some(CompletionItemKind::CONSTRUCTOR),
        detail: Some(
            "Optional object attribute — omitted ⇒ null unless a default is given".to_string(),
        ),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: tfls_core::optional_description().to_string(),
        })),
        insert_text: Some("optional(${1:string})".to_string()),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        sort_text: Some("02_optional".to_string()),
        ..Default::default()
    });
    items
}

/// Render completion items for a `BuiltinSchema` — used by
/// `terraform {}`, `variable {}`, `output {}`, and each backend
/// schema, all of which have a hand-maintained static table.
fn builtin_body_items(
    schema: tfls_core::builtin_blocks::BuiltinSchema,
    filter: &BodyFilter,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = schema
        .attrs
        .iter()
        .filter(|a| !filter.present_attrs.contains(a.name))
        .map(|a| {
            // One attribute per completion item. Previously this
            // branch auto-appended companion attrs for
            // `variable.type` (→ `default` + `description`) and
            // `output.value` (→ `description`), but that meant
            // picking `type` in an already-written block dumped
            // duplicate `default` / `description` lines into the
            // user's source. The top-level scaffold snippets
            // (`variable "x" { … }`, `output "x" { … }`) still
            // pre-fill the common attrs; the per-attr body items
            // do exactly what the label says.
            let insert_text = format!("{name} = ${{1}}", name = a.name);
            CompletionItem {
                label: a.name.to_string(),
                kind: Some(if a.required {
                    CompletionItemKind::FIELD
                } else {
                    CompletionItemKind::PROPERTY
                }),
                // Required attrs first (bucket 0), optional next (1).
                sort_text: Some(format!("{}_{}", if a.required { 0 } else { 1 }, a.name)),
                detail: Some(a.detail.to_string()),
                insert_text: Some(insert_text),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect();
    for b in schema.blocks {
        if filter.block_present(b.name) {
            continue;
        }
        items.push(CompletionItem {
            label: b.name.to_string(),
            kind: Some(CompletionItemKind::STRUCT),
            // Nested blocks after attributes (bucket 2).
            sort_text: Some(format!("2_{}", b.name)),
            detail: Some(b.detail.to_string()),
            insert_text: Some(render_block_snippet(b)),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    items
}

/// Build the snippet body for a nested built-in block. Handles:
/// - labeled vs unlabeled headers (`backend "s3" { … }` vs
///   `required_providers { … }`)
/// - pre-filled required attributes with numbered tabstops so the
///   user can tab through them instead of landing in an empty block
/// - final `$0` tabstop at the end of the body for free-form edits
fn render_block_snippet(b: &tfls_core::builtin_blocks::BuiltinBlock) -> String {
    let (header, mut next_tab) = match b.label_placeholder {
        Some(placeholder) => (
            format!(
                "{name} \"${{1:{placeholder}}}\" {{",
                name = b.name,
                placeholder = placeholder,
            ),
            2,
        ),
        None => (format!("{name} {{", name = b.name), 1),
    };
    let mut body = String::new();
    for ra in b.required_attrs {
        if ra.quoted {
            body.push_str(&format!("\n  {name} = \"${{{next_tab}}}\"", name = ra.name));
        } else {
            body.push_str(&format!("\n  {name} = ${{{next_tab}}}", name = ra.name));
        }
        next_tab += 1;
    }
    // Only emit a trailing `$0` line for empty-body snippets. With
    // required attrs filled in, an extra `\n  $0\n` would leave a
    // blank whitespace line before `}` after tab-through. Without
    // `$0`, the cursor naturally lands at the end of the snippet
    // (past `}`) once the user tabs off the last required attr —
    // the more common next step anyway.
    if b.required_attrs.is_empty() {
        body.push_str("\n  $0\n}");
    } else {
        body.push_str("\n}");
    }
    format!("{header}{body}")
}

/// Completions for the body of a `provider "x" { ... }` block. Uses
/// the loaded provider schema's top-level `provider: Schema` field,
/// plus `alias` (the universal meta-argument).
fn provider_block_body_items(
    backend: &Backend,
    provider_local_name: &str,
    filter: &BodyFilter,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    // Schema attributes: find the provider whose address type matches
    // the local name used in `provider "<local>"`. This mirrors how
    // resource schemas are looked up by unqualified type name.
    let provider_schema = backend
        .state
        .schemas
        .iter()
        .find(|e| e.key().r#type == provider_local_name)
        .map(|e| std::sync::Arc::clone(e.value()));
    if let Some(ps) = provider_schema {
        for (name, attr) in &ps.provider.block.attributes {
            if filter.present_attrs.contains(name.as_str()) {
                continue;
            }
            if !(attr.required || attr.optional) {
                continue;
            }
            items.push(CompletionItem {
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
                insert_text: Some(schema_attribute_insert_text(name, attr)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        }
    }
    // Universal provider meta-arguments.
    for a in builtin_blocks::PROVIDER_BLOCK_META_ATTRS {
        if filter.present_attrs.contains(a.name) {
            continue;
        }
        items.push(CompletionItem {
            label: a.name.to_string(),
            kind: Some(CompletionItemKind::PROPERTY),
            detail: Some(a.detail.to_string()),
            documentation: Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: a.detail.to_string(),
            })),
            insert_text: Some(format!("{name} = ${{1}}", name = a.name)),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Items for `required_providers { | }` — each common provider local
/// name as a scaffold that expands to the full `NAME = { source = "…",
/// version = "…" }` entry. The version tabstop's default text is the
/// latest published `~> MAJOR.MINOR` from the cached registry version
/// list when available; falls back to an empty tabstop when the
/// cache is cold (so the user lands on `version = ""` and the
/// per-version completion path takes over). The previous template
/// hard-coded `~> 1.0`, which on long-lived providers like azurerm
/// (latest 4.x) silently anchored every new project at the original
/// 2018-era major version.
fn required_providers_entry_items(filter: &BodyFilter) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    for (local_name, source, hint) in builtin_blocks::REQUIRED_PROVIDERS_COMMON_ENTRIES {
        if filter.present_attrs.contains(*local_name) {
            continue;
        }
        // `source` is "<namespace>/<name>" — every entry in
        // REQUIRED_PROVIDERS_COMMON_ENTRIES uses this two-part form.
        let (ns, name) = match source.split_once('/') {
            Some(parts) => parts,
            None => continue,
        };
        let version_tabstop =
            match tfls_provider_protocol::registry_versions::cached_latest_version(ns, name)
                .as_deref()
                .and_then(tfls_provider_protocol::registry_versions::major_minor_of)
            {
                // Cached MM available → bake `~> 4.71` (or whatever) into
                // the placeholder so a tab-and-accept lands on a sensible
                // default tracking the current major.
                Some(mm) => format!("${{1:~> {mm}}}"),
                // Cold cache → empty tabstop. The version-value
                // completion path (`required_provider_version_value_items`)
                // fires the moment the user triggers completion inside
                // the empty quotes; preselects latest.
                None => "${1}".to_string(),
            };
        items.push(CompletionItem {
            label: local_name.to_string(),
            kind: Some(CompletionItemKind::MODULE),
            detail: Some(format!("{} — source {}", hint, source)),
            insert_text: Some(format!(
                "{local_name} = {{\n  source  = \"{source}\"\n  version = \"{version_tabstop}\"\n}}$0"
            )),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Items for `source = "|"` inside a `required_providers` entry.
///
/// Three sources of items, in priority order via `sortText`:
///   1. Curated entries (hand-maintained in `builtin_blocks`), each
///      shown in three flavours: bare, `registry.terraform.io/`-
///      prefixed, and `registry.opentofu.org/`-prefixed — so users
///      can pin a specific registry when needed.
///   2. Live Terraform registry catalog (official + partner tiers,
///      ~250 providers), cached 7 days. Fetched via the shared
///      `registry_catalog` module.
///   3. A curated entry always wins ordering / de-dupe against a
///      catalog entry for the same source, so popular ones stay at
///      the top.
///
/// Community tier is deliberately excluded — thousands of rarely-
/// used providers would dominate the list with noise.
async fn required_provider_source_value_items() -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // (1) Curated entries — `00_` sort prefix keeps them on top.
    for (_, source, hint) in builtin_blocks::REQUIRED_PROVIDERS_COMMON_ENTRIES {
        let bare = (*source).to_string();
        let tf_prefixed = format!("registry.terraform.io/{source}");
        let tofu_prefixed = format!("registry.opentofu.org/{source}");
        if seen.insert(bare.clone()) {
            items.push(CompletionItem {
                label: bare.clone(),
                sort_text: Some(format!("00_{bare}")),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(format!("default registry — {hint}")),
                insert_text: Some(bare),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                ..Default::default()
            });
        }
        if seen.insert(tf_prefixed.clone()) {
            items.push(CompletionItem {
                label: tf_prefixed.clone(),
                sort_text: Some(format!("00_{tf_prefixed}")),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(format!("Terraform registry — {hint}")),
                insert_text: Some(tf_prefixed),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                ..Default::default()
            });
        }
        if seen.insert(tofu_prefixed.clone()) {
            items.push(CompletionItem {
                label: tofu_prefixed.clone(),
                sort_text: Some(format!("00_{tofu_prefixed}")),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(format!("OpenTofu registry — {hint}")),
                insert_text: Some(tofu_prefixed),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                ..Default::default()
            });
        }
    }

    // (2) Live catalog — official + partner tier providers. Failures
    // are absorbed; we still show the curated list on top so source
    // completion is never empty.
    if let Ok(client) = tfls_provider_protocol::registry_catalog::build_http_client() {
        match tfls_provider_protocol::registry_catalog::fetch_catalog(&client).await {
            Ok(catalog) => {
                for entry in catalog {
                    let source = entry.source();
                    if !seen.insert(source.clone()) {
                        continue; // already emitted by the curated list
                    }
                    let tier_label = entry
                        .tier
                        .as_deref()
                        .map(|t| format!("{} tier", t))
                        .unwrap_or_else(|| "public".to_string());
                    let detail = match entry.description.as_deref() {
                        Some(d) if !d.trim().is_empty() => {
                            format!("{} — {}", tier_label, d.trim())
                        }
                        _ => tier_label,
                    };
                    items.push(CompletionItem {
                        label: source.clone(),
                        sort_text: Some(format!("01_{source}")),
                        kind: Some(CompletionItemKind::VALUE),
                        detail: Some(detail),
                        insert_text: Some(source),
                        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                        ..Default::default()
                    });
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "catalog fetch failed; only curated sources will show");
            }
        }
    }

    items
}

/// Completion items for every version-constraint operator, each
/// carrying its shared `short_description` / `long_description` copy
/// from `tfls_core::version_constraint`. Rendered inline (`detail`)
/// and on hover (`documentation`).
fn constraint_operator_items() -> Vec<CompletionItem> {
    use tfls_core::version_constraint::{ConstraintOp, ALL_OPERATORS};
    ALL_OPERATORS
        .iter()
        .map(|op| {
            let token = op.token();
            CompletionItem {
                label: token.to_string(),
                kind: Some(CompletionItemKind::OPERATOR),
                detail: Some(op.short_description().to_string()),
                documentation: Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: op.long_description().to_string(),
                })),
                insert_text: Some(format!("{token} ${{1}}")),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                // Sort by preferred order — `>=` and `~>` near the top.
                sort_text: Some(match *op {
                    ConstraintOp::Gte => "00".to_string(),
                    ConstraintOp::Pessimistic => "01".to_string(),
                    ConstraintOp::Eq => "02".to_string(),
                    ConstraintOp::Ne => "03".to_string(),
                    ConstraintOp::Gt => "04".to_string(),
                    ConstraintOp::Lt => "05".to_string(),
                    ConstraintOp::Lte => "06".to_string(),
                }),
                ..Default::default()
            }
        })
        .collect()
}

/// Items for `version = "|"` inside a `required_providers` entry.
/// Constraint-aware: at the start of an empty constraint (or after
/// a comma) we pre-fill the latest version + its common
/// constraint-prefixed variants so the user can pick a flavour
/// without having to type a digit first; mid-version we just list
/// the matching exact versions.
async fn required_provider_version_value_items(
    client: &reqwest::Client,
    source: Option<&str>,
    cursor_partial: &str,
) -> Vec<CompletionItem> {
    use tfls_core::version_constraint::{cursor_slot, CursorSlot};
    let slot = cursor_slot(cursor_partial, cursor_partial.len());
    match slot {
        CursorSlot::AtOperator | CursorSlot::Trailing => {
            // Pre-filled flavours (latest exact, `~> MM`, `>= latest`, …)
            // sit on top, followed by raw operator items + the
            // remaining exact versions. If the registry fetch failed
            // (cold cache, network down) we still show operators —
            // better than empty.
            let mut items = prefilled_provider_version_items(client, source).await;
            if items.is_empty() {
                items = constraint_operator_items();
            }
            items
        }
        CursorSlot::AfterOperator(_) | CursorSlot::InsideVersion { .. } => {
            let vp = version_partial_of(&slot);
            let mut items = provider_version_items_from_registry(client, source, vp).await;
            if items.is_empty() {
                items = constraint_operator_items();
            }
            items
        }
    }
}

/// Build the "empty constraint" suggestion list for a provider:
/// latest exact, `~> MAJOR.MINOR`, `>= latest`, then the operator
/// scaffolds, then all known versions. Sort-keys keep the latest
/// flavours pinned at the top regardless of how the client
/// alphabetises operator + version labels.
async fn prefilled_provider_version_items(
    client: &reqwest::Client,
    source: Option<&str>,
) -> Vec<CompletionItem> {
    let Some((ns, name)) = source.and_then(parse_source) else {
        // No source → can't fetch — fall back to operator-only.
        return constraint_operator_items();
    };
    let versions =
        match tfls_provider_protocol::registry_versions::fetch_versions(client, &ns, &name).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "registry version fetch failed");
                return constraint_operator_items();
            }
        };
    // Preselect the latest STABLE release as the default — `versions` is
    // major-dominant descending, so `first()` would pin a higher-major
    // pre-release (e.g. `6.0.0-beta1` over stable `5.99.0`). Fall back to
    // the absolute latest only when every version is a pre-release.
    let Some(latest) = versions
        .iter()
        .find(|v| !v.version.contains('-'))
        .or_else(|| versions.first())
    else {
        return constraint_operator_items();
    };
    let latest_v = latest.version.clone();
    let provenance = latest.provenance_label();

    let mut items: Vec<CompletionItem> = Vec::new();

    // 0000 — bare latest exact: `5.94.0`. Most common pick.
    items.push(CompletionItem {
        label: latest_v.clone(),
        kind: Some(CompletionItemKind::VALUE),
        detail: Some(format!("latest — {ns}/{name} ({provenance})")),
        insert_text: Some(latest_v.clone()),
        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
        sort_text: Some("0000_latest".to_string()),
        preselect: Some(true),
        ..Default::default()
    });

    // 0001 — `~> MAJOR.MINOR` of the latest. Pessimistic upgrade
    // band; the recommended default per Terraform docs.
    if let Some(mm) = major_minor(&latest_v) {
        let label = format!("~> {mm}");
        items.push(CompletionItem {
            label: label.clone(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("pessimistic (compatible) constraint to latest".to_string()),
            insert_text: Some(label),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            sort_text: Some("0001_pessimistic".to_string()),
            ..Default::default()
        });
    }

    // 0002 — `>= LATEST` (lower-bound, no upper).
    let gte = format!(">= {latest_v}");
    items.push(CompletionItem {
        label: gte.clone(),
        kind: Some(CompletionItemKind::SNIPPET),
        detail: Some("at least latest, any future".to_string()),
        insert_text: Some(gte),
        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
        sort_text: Some("0002_gte_latest".to_string()),
        ..Default::default()
    });

    // 1xxx — raw operator scaffolds (preserve existing UX for
    // users who want to start with `~>` and pick a different
    // version themselves).
    for mut op in constraint_operator_items() {
        if let Some(s) = op.sort_text.take() {
            op.sort_text = Some(format!("1{s}"));
        }
        items.push(op);
    }

    // 2xxx — every known exact version, descending. Latest is
    // duplicated at 0000 above; keep it here too so users
    // searching by typing digits still find it.
    for (idx, vi) in versions.iter().enumerate() {
        items.push(CompletionItem {
            label: vi.version.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format!("{ns}/{name} — {}", vi.provenance_label())),
            insert_text: Some(vi.version.clone()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            sort_text: Some(format!("2{idx:04}")),
            ..Default::default()
        });
    }

    items
}

/// Extract the version digits the user has already typed (`>= 4.7` →
/// `4.7`), so the registry list can be pre-filtered. Empty for the
/// AfterOperator slot (operator typed, no digits yet).
fn version_partial_of(slot: &tfls_core::version_constraint::CursorSlot) -> &str {
    use tfls_core::version_constraint::CursorSlot;
    match slot {
        CursorSlot::InsideVersion { partial, .. } => partial.as_str(),
        _ => "",
    }
}

/// Pull exact-version items for a given provider source from the
/// Terraform + OpenTofu registries, pre-filtered to the typed prefix.
async fn provider_version_items_from_registry(
    client: &reqwest::Client,
    source: Option<&str>,
    version_partial: &str,
) -> Vec<CompletionItem> {
    let Some((ns, name)) = source.and_then(parse_source) else {
        return Vec::new();
    };
    let versions =
        match tfls_provider_protocol::registry_versions::fetch_versions(client, &ns, &name).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "registry version fetch failed");
                return Vec::new();
            }
        };
    // Pre-filter to versions matching the typed prefix so clients without
    // label-prefix matching don't show the full 1000+ list each keystroke.
    // Fall back to the full list when nothing matches (typo / new digit).
    let mut matches: Vec<&_> = versions
        .iter()
        .filter(|vi| vi.version.starts_with(version_partial))
        .collect();
    if matches.is_empty() {
        matches = versions.iter().collect();
    }
    let mut items: Vec<CompletionItem> = Vec::new();
    for vi in matches {
        items.push(CompletionItem {
            label: vi.version.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format!("{ns}/{name} — {}", vi.provenance_label())),
            insert_text: Some(vi.version.clone()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        });
    }
    // No `~> MM` operator templates here: this builder only serves the
    // AfterOperator / InsideVersion slots (the user already typed an
    // operator or version digits), where prepending another operator
    // would produce `>= ~> 4.71`. Operator flavours live in the
    // AtOperator path (prefilled_provider_version_items).
    items
}

/// Items for `required_version = "|"` inside a top-level `terraform {}`
/// block. At an empty constraint we pre-fill the latest CLI release +
/// common operator-prefixed flavours so `Tab` / `Enter` lands on a
/// usable value immediately. After an operator we fall through to
/// the existing exact-version list.
async fn required_version_value_items(cursor_partial: &str) -> Vec<CompletionItem> {
    use tfls_core::version_constraint::{cursor_slot, CursorSlot};
    let slot = cursor_slot(cursor_partial, cursor_partial.len());
    match slot {
        CursorSlot::AtOperator | CursorSlot::Trailing => {
            let mut items = prefilled_tool_version_items().await;
            if items.is_empty() {
                items = constraint_operator_items();
            }
            items
        }
        CursorSlot::AfterOperator(_) | CursorSlot::InsideVersion { .. } => {
            let mut items = tool_version_items_from_github(version_partial_of(&slot)).await;
            if items.is_empty() {
                items = constraint_operator_items();
            }
            items
        }
    }
}

/// Mirror of [`prefilled_provider_version_items`] for the combined
/// Terraform and OpenTofu CLI release catalogue. Pins the latest
/// release at sort-key `0000_latest`, then offers the pessimistic and
/// lower-bound flavours (`~> MAJOR.MINOR`, `>= LATEST`), then the
/// operator scaffolds, then every known exact release for users who
/// want to scroll-and-pick.
async fn prefilled_tool_version_items() -> Vec<CompletionItem> {
    let Ok(client) = tfls_provider_protocol::tool_versions::build_http_client() else {
        return constraint_operator_items();
    };
    let versions = match tfls_provider_protocol::tool_versions::fetch_tool_versions(&client).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "github release fetch failed");
            return constraint_operator_items();
        }
    };
    // Latest STABLE release as the preselected default (skip pre-releases
    // unless that's all that exists) — see prefilled_provider_version_items.
    let Some(latest) = versions
        .iter()
        .find(|v| !v.version.contains('-'))
        .or_else(|| versions.first())
    else {
        return constraint_operator_items();
    };
    let latest_v = latest.version.clone();
    let provenance = latest.provenance_label();

    let mut items: Vec<CompletionItem> = Vec::new();
    items.push(CompletionItem {
        label: latest_v.clone(),
        kind: Some(CompletionItemKind::VALUE),
        detail: Some(format!("latest CLI release ({provenance})")),
        insert_text: Some(latest_v.clone()),
        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
        sort_text: Some("0000_latest".to_string()),
        preselect: Some(true),
        ..Default::default()
    });
    if let Some(mm) = major_minor(&latest_v) {
        let label = format!("~> {mm}");
        items.push(CompletionItem {
            label: label.clone(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("pessimistic (compatible) constraint to latest".to_string()),
            insert_text: Some(label),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            sort_text: Some("0001_pessimistic".to_string()),
            ..Default::default()
        });
    }
    let gte = format!(">= {latest_v}");
    items.push(CompletionItem {
        label: gte.clone(),
        kind: Some(CompletionItemKind::SNIPPET),
        detail: Some("at least latest, any future".to_string()),
        insert_text: Some(gte),
        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
        sort_text: Some("0002_gte_latest".to_string()),
        ..Default::default()
    });
    for mut op in constraint_operator_items() {
        if let Some(s) = op.sort_text.take() {
            op.sort_text = Some(format!("1{s}"));
        }
        items.push(op);
    }
    for (idx, vi) in versions.iter().enumerate() {
        items.push(CompletionItem {
            label: vi.version.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format!("CLI release — {}", vi.provenance_label())),
            insert_text: Some(vi.version.clone()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            sort_text: Some(format!("2{idx:04}")),
            ..Default::default()
        });
    }
    items
}

async fn tool_version_items_from_github(version_partial: &str) -> Vec<CompletionItem> {
    let Ok(client) = tfls_provider_protocol::tool_versions::build_http_client() else {
        return Vec::new();
    };
    let versions = match tfls_provider_protocol::tool_versions::fetch_tool_versions(&client).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "github release fetch failed");
            return Vec::new();
        }
    };
    let mut matches: Vec<&_> = versions
        .iter()
        .filter(|vi| vi.version.starts_with(version_partial))
        .collect();
    if matches.is_empty() {
        matches = versions.iter().collect();
    }
    let mut items: Vec<CompletionItem> = Vec::new();
    for vi in matches {
        items.push(CompletionItem {
            label: vi.version.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format!("CLI release — {}", vi.provenance_label())),
            insert_text: Some(vi.version.clone()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        });
    }
    // Operator templates omitted — see provider_version_items_from_registry.
    items
}

/// Items for `version = "|"` inside a `module "…" { … }` block.
/// Constraint-aware: operators at the start / after comma, real
/// module registry versions after an operator (when the module's
/// `source` is a registry path like `ns/name/provider`).
async fn module_version_value_items(
    client: &reqwest::Client,
    source: Option<&str>,
    cursor_partial: &str,
) -> Vec<CompletionItem> {
    use tfls_core::version_constraint::{cursor_slot, CursorSlot};
    let slot = cursor_slot(cursor_partial, cursor_partial.len());
    match slot {
        CursorSlot::AtOperator | CursorSlot::Trailing => constraint_operator_items(),
        CursorSlot::AfterOperator(_) | CursorSlot::InsideVersion { .. } => {
            let mut items =
                module_version_items_from_registry(client, source, version_partial_of(&slot)).await;
            if items.is_empty() {
                items = constraint_operator_items();
            }
            items
        }
    }
}

async fn module_version_items_from_registry(
    client: &reqwest::Client,
    source: Option<&str>,
    version_partial: &str,
) -> Vec<CompletionItem> {
    let Some((ns, name, provider)) = source.and_then(parse_module_source) else {
        return Vec::new();
    };
    let versions = match tfls_provider_protocol::registry_versions::fetch_module_versions(
        client, &ns, &name, &provider,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "module registry version fetch failed");
            return Vec::new();
        }
    };
    let mut matches: Vec<&_> = versions
        .iter()
        .filter(|vi| vi.version.starts_with(version_partial))
        .collect();
    if matches.is_empty() {
        matches = versions.iter().collect();
    }
    let mut items: Vec<CompletionItem> = Vec::new();
    for vi in matches {
        items.push(CompletionItem {
            label: vi.version.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format!(
                "{ns}/{name}/{provider} — {}",
                vi.provenance_label()
            )),
            insert_text: Some(vi.version.clone()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        });
    }
    // Operator templates omitted — see provider_version_items_from_registry.
    items
}

/// Split a registry module source `"ns/name/provider"` into its
/// three components. Returns `None` for non-registry sources (git
/// URLs, local paths, etc.).
fn parse_module_source(s: &str) -> Option<(String, String, String)> {
    let s = s.trim_matches('"').trim();
    // Non-registry forms never start with a bare namespace.
    if s.starts_with('.') || s.starts_with('/') || s.contains("://") || s.contains("::") {
        return None;
    }
    let parts: Vec<&str> = s.split('/').collect();
    match parts.as_slice() {
        [ns, name, provider] if !ns.is_empty() && !name.is_empty() && !provider.is_empty() => {
            Some((ns.to_string(), name.to_string(), provider.to_string()))
        }
        _ => None,
    }
}

/// Split `"namespace/name"` into its two components. Returns `None`
/// for anything that doesn't match the registry shape.
fn parse_source(s: &str) -> Option<(String, String)> {
    let s = s.trim_matches('"').trim();
    let mut parts = s.splitn(3, '/');
    let a = parts.next()?;
    let b = parts.next()?;
    // Registries also accept `host/ns/name`; in that form the first
    // component is the host, last two are what we want.
    if let Some(c) = parts.next() {
        return Some((b.to_string(), c.to_string()));
    }
    Some((a.to_string(), b.to_string()))
}

/// Extract `"X.Y"` from `"X.Y.Z"` (or `"X.Y.Z-pre"` etc.).
fn major_minor(version: &str) -> Option<String> {
    let core = version.split('-').next().unwrap_or(version);
    let mut it = core.splitn(3, '.');
    let major = it.next()?;
    let minor = it.next()?;
    if major.is_empty() || minor.is_empty() {
        return None;
    }
    Some(format!("{major}.{minor}"))
}

/// Items for the object-literal body of a `required_providers` entry,
/// e.g. `aws = { | }`. Suggests `source`, `version`,
/// `configuration_aliases`.
fn required_provider_entry_attr_items(filter: &BodyFilter) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = builtin_blocks::REQUIRED_PROVIDER_ENTRY_ATTRS
        .iter()
        .filter(|a| !filter.present_attrs.contains(a.name))
        .map(|a| CompletionItem {
            label: a.name.to_string(),
            kind: Some(if a.required {
                CompletionItemKind::FIELD
            } else {
                CompletionItemKind::PROPERTY
            }),
            detail: Some(a.detail.to_string()),
            insert_text: Some(format!("{name} = \"${{1}}\"", name = a.name)),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
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
    //
    // A step may also name a "block-like" attribute — a cty
    // object / collection-of-object that a provider exposes as an
    // attribute but Terraform lets authors write with block / `dynamic`
    // syntax (e.g. azurerm managed_disk = set(object(...))). Once we step
    // into one, completion comes from the cty object's fields, not a
    // `BlockSchema`.
    let mut block_schema = &schema.block;
    let mut object_node: Option<&sonic_rs::Value> = None;
    for step in nested_path {
        if let Some(node) = object_node {
            // Already inside a cty object: the next step must be a field
            // whose own type is object / collection-of-object.
            match field_type_in_object(node, step).and_then(block_like_object_node) {
                Some(inner) => object_node = Some(inner),
                None => return Vec::new(),
            }
            continue;
        }
        match block_schema.block_types.get(step) {
            Some(nb) => block_schema = &nb.block,
            None => {
                match block_schema
                    .attributes
                    .get(step)
                    .filter(|a| a.is_block_like())
                    .and_then(|a| a.r#type.as_ref())
                    .and_then(block_like_object_node)
                {
                    Some(node) => object_node = Some(node),
                    None => return Vec::new(),
                }
            }
        }
    }

    // Landed inside a block-like attribute: offer its object fields.
    if let Some(node) = object_node {
        return object_field_items(node, filter);
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
            !filter.present_attrs.contains(name.as_str()) && (attr.required || attr.optional)
        })
        .map(|(name, attr)| CompletionItem {
            label: name.clone(),
            kind: Some(if attr.required {
                CompletionItemKind::FIELD
            } else {
                CompletionItemKind::PROPERTY
            }),
            // Required attrs sort above optional ones so the must-fill
            // fields (e.g. aws_instance.ami) sit at the top of the menu
            // instead of being buried alphabetically among optionals.
            // Deprecated attrs sink to a late bucket and carry the
            // DEPRECATED tag (clients strike them through) — kept, not
            // dropped, so legacy configs can still complete them.
            sort_text: Some(format!(
                "{}_{name}",
                if attr.deprecated {
                    8
                } else if attr.required {
                    0
                } else {
                    1
                }
            )),
            tags: deprecated_tags(attr.deprecated),
            detail: Some(attribute_detail(attr)),
            documentation: attr.description.as_ref().map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d.clone(),
                })
            }),
            insert_text: Some(schema_attribute_insert_text(name, attr)),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect();

    items.extend(
        block_schema
            .block_types
            .iter()
            .filter(|(name, nb)| {
                // Suppress a nested block that's at its cardinality limit:
                // a `single` block already placed, or a List/Set block
                // already present `max_items` times (providers encode
                // "max one" as List/Set + max_items=1, e.g.
                // root_block_device). max_items == 0 means unbounded.
                let count = filter.block_count(name);
                if nb.nesting_mode == NestingMode::Single {
                    return count == 0;
                }
                nb.max_items == 0 || count < nb.max_items as usize
            })
            .map(|(name, nb)| CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some(if nb.block.deprecated {
                    "nested block — deprecated".to_string()
                } else {
                    "nested block".to_string()
                }),
                // Nested blocks sort after attributes (bucket 2);
                // deprecated ones sink to bucket 9.
                sort_text: Some(format!(
                    "{}_{name}",
                    if nb.block.deprecated { 9 } else { 2 }
                )),
                tags: deprecated_tags(nb.block.deprecated),
                insert_text: Some(nested_block_scaffold_snippet(name, &nb.block)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }),
    );

    // Meta-arguments (`count`, `for_each`, `lifecycle`, …) are valid
    // only at the top level of a resource or data body — not inside
    // nested schema blocks like `root_block_device` or `lifecycle`.
    if nested_path.is_empty() {
        let kind = if data {
            BlockKind::Data
        } else {
            BlockKind::Resource
        };
        items.extend(
            meta_argument_items(kind)
                .into_iter()
                .filter(|item| !filter.present_attrs.contains(&item.label)),
        );
        items.extend(meta_block_items(kind).into_iter().filter(|item| {
            !(is_singleton_meta_block(kind, &item.label) && filter.block_present(&item.label))
        }));
    }

    // Meta items carry no sort_text — drop them into the last bucket so
    // their unprefixed labels don't sort ahead of schema attributes.
    for item in &mut items {
        if item.sort_text.is_none() {
            item.sort_text = Some(format!("3_{}", item.label));
        }
    }
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    items
}

/// Peel collection wrappers off a cty type and return its underlying
/// `["object", {fields}, [optional]?]` node, or `None` if the type isn't
/// an object / collection-of-object.
fn block_like_object_node(t: &sonic_rs::Value) -> Option<&sonic_rs::Value> {
    use sonic_rs::JsonValueTrait;
    match t.get(0).and_then(|v| v.as_str())? {
        "object" => Some(t),
        "set" | "list" | "map" => block_like_object_node(t.get(1)?),
        _ => None,
    }
}

/// The cty type of field `name` inside an object node
/// (`["object", {fields}, …]`).
fn field_type_in_object<'a>(node: &'a sonic_rs::Value, name: &str) -> Option<&'a sonic_rs::Value> {
    use sonic_rs::JsonValueTrait;
    node.get(1).and_then(|m| m.get(name))
}

/// Completion items for the fields of a cty object node. Fields not listed
/// in the object's optional set (3rd element) are required and sort first.
fn object_field_items(node: &sonic_rs::Value, filter: &BodyFilter) -> Vec<CompletionItem> {
    use sonic_rs::{JsonContainerTrait, JsonValueTrait};
    let Some(map) = node.get(1).and_then(|m| m.as_object()) else {
        return Vec::new();
    };
    let optionals: HashSet<&str> = node
        .get(2)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let mut items: Vec<CompletionItem> = map
        .iter()
        .filter(|(name, _)| !filter.present_attrs.contains(*name))
        .map(|(name, _ty)| {
            let required = !optionals.contains(name);
            CompletionItem {
                label: name.to_string(),
                kind: Some(if required {
                    CompletionItemKind::FIELD
                } else {
                    CompletionItemKind::PROPERTY
                }),
                sort_text: Some(format!("{}_{name}", u8::from(!required))),
                detail: Some(
                    if required {
                        "attribute (required)"
                    } else {
                        "attribute"
                    }
                    .to_string(),
                ),
                insert_text: Some(format!("{name} = ")),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                ..Default::default()
            }
        })
        .collect();
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    items
}

/// Attributes and nested blocks already present in the enclosing
/// block at the completion cursor, used to suppress duplicate
/// suggestions.
#[derive(Default)]
struct BodyFilter {
    present_attrs: HashSet<String>,
    /// Count of each literal nested block already present (a `dynamic`
    /// block keys on `"dynamic"`, not its target, so it never counts
    /// toward the target's max_items).
    present_blocks: HashMap<String, usize>,
}

impl BodyFilter {
    fn block_present(&self, name: &str) -> bool {
        self.present_blocks.contains_key(name)
    }
    fn block_count(&self, name: &str) -> usize {
        self.present_blocks.get(name).copied().unwrap_or(0)
    }
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
            *out.present_blocks
                .entry(nested.ident.as_str().to_string())
                .or_insert(0) += 1;
        }
    }
    out
}

/// True when the cursor sits directly in the body of a
/// `dynamic "<label>" { … }` block — outside its `content { }`
/// child. Completion at this position should offer only the
/// dynamic meta-args and the `content` scaffold, not the target
/// nested block's attrs (which belong inside `content`).
fn cursor_on_dynamic_body(body: Option<&Body>, offset: usize) -> bool {
    body.and_then(|b| innermost_block_at(b, offset))
        .is_some_and(|b| b.ident.as_str() == "dynamic")
}

fn innermost_block_at(body: &Body, offset: usize) -> Option<&Block> {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if !span_contains_offset(block.span(), offset) {
            continue;
        }
        // Only descend if the cursor is actually inside the body
        // (past the opening `{`). A cursor on the block header —
        // e.g. on the `c` of `condition {` — belongs to the enclosing
        // block's body, not to the nested block's body.
        if cursor_in_block_body(block, offset) {
            // For `dynamic "<label>" { content { … } }` the
            // innermost body we care about for filtering is the
            // content body (if the cursor is inside it) — that's
            // where present_attrs live from the user's POV.
            if block.ident.as_str() == "dynamic" {
                if let Some(content) = find_content_child(&block.body, offset) {
                    return Some(innermost_block_at(&content.body, offset).unwrap_or(content));
                }
            }
            return Some(innermost_block_at(&block.body, offset).unwrap_or(block));
        }
        return None;
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
        if span_contains_offset(block.span(), offset) && cursor_in_block_body(block, offset) {
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
        // A cursor on the nested block's header line (before the `{`)
        // is still "in the parent body". Stop descending so the caller
        // suggests sibling block names instead of the nested block's
        // own attrs — which would otherwise all be filtered out as
        // "already present".
        if !cursor_in_block_body(nested, offset) {
            return;
        }
        let ident = nested.ident.as_str();
        if ident == "dynamic" {
            // A `dynamic "<label>" {}` is a meta-construct that
            // generates instances of `<label>` at plan time. For
            // schema-lookup purposes treat it as if it were a
            // plain `<label> { content { … } }` — push the label
            // onto the path (so the target block's schema is
            // resolved), then look through the `content {}`
            // wrapper if the cursor is inside it.
            let Some(label) = nested.labels.first().map(block_label_text) else {
                // Malformed dynamic (no label) — don't descend.
                return;
            };
            path.push(label);
            // The content {} child is a schema-less wrapper —
            // step through its body without pushing onto the path.
            if let Some(content) = find_content_child(&nested.body, offset) {
                collect_nested_path(&content.body, offset, path);
            }
            return;
        }
        path.push(ident.to_string());
        collect_nested_path(&nested.body, offset, path);
        return;
    }
}

/// Find a `content { }` child inside a dynamic-block body that
/// contains `offset`. Returns `None` when the cursor is elsewhere
/// in the dynamic body (e.g. on `for_each = …`), so the caller
/// can stop descending at the dynamic level and let
/// `DynamicBlockBody` classification kick in.
fn find_content_child(body: &Body, offset: usize) -> Option<&Block> {
    for structure in body.iter() {
        let Some(child) = structure.as_block() else {
            continue;
        };
        if child.ident.as_str() != "content" {
            continue;
        }
        if cursor_in_block_body(child, offset) {
            return Some(child);
        }
    }
    None
}

/// Extract the text value of a block label — strings get their
/// inner text, identifiers get their raw chars.
fn block_label_text(label: &hcl_edit::structure::BlockLabel) -> String {
    match label {
        hcl_edit::structure::BlockLabel::String(s) => s.value().to_string(),
        hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
    }
}

fn span_contains_offset(span: Option<std::ops::Range<usize>>, offset: usize) -> bool {
    matches!(span, Some(r) if offset >= r.start && offset <= r.end)
}

/// True when `offset` lies inside the block's body (between `{` and `}`).
/// Used to distinguish cursors on the block header line from cursors
/// inside the body — they completion-classify differently.
fn cursor_in_block_body(block: &Block, offset: usize) -> bool {
    match block.body.span() {
        Some(body_span) => offset > body_span.start && offset <= body_span.end,
        // hcl-edit leaves body span unset for an empty body (`foo {}`).
        // Fall back to the whole block span — same behaviour as before,
        // good enough because there are no body contents to classify
        // against anyway.
        None => span_contains_offset(block.span(), offset),
    }
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
                documentation: meta_arg_documentation(name),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect()
}

/// Items for a cursor sitting directly in the body of a
/// `dynamic "<label>" { … }` block — not inside `content { }`.
/// The dynamic body accepts only the three meta-args
/// (`for_each` required, `iterator` / `labels` optional) and a
/// single `content { }` sub-block. Already-present items are
/// filtered out so repeat suggestions don't clutter the menu.
fn dynamic_body_items(filter: &BodyFilter) -> Vec<CompletionItem> {
    let meta = [
        ("for_each", "meta-argument — required", "for_each = ${1}"),
        (
            "iterator",
            "meta-argument — rename `each`",
            "iterator = \"${1}\"",
        ),
        ("labels", "meta-argument — labels list", "labels = [${1}]"),
    ];
    let mut items: Vec<CompletionItem> = meta
        .iter()
        .filter(|(name, _, _)| !filter.present_attrs.contains(*name))
        .map(|(name, detail, snippet)| CompletionItem {
            label: (*name).to_string(),
            kind: Some(CompletionItemKind::PROPERTY),
            detail: Some((*detail).to_string()),
            documentation: dynamic_meta_attr_documentation(name),
            insert_text: Some((*snippet).to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect();

    if !filter.block_present("content") {
        items.push(CompletionItem {
            label: "content".to_string(),
            kind: Some(CompletionItemKind::STRUCT),
            detail: Some("meta-block — body template".to_string()),
            documentation: Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: tfls_core::content_meta_block_description().to_string(),
            })),
            insert_text: Some("content {\n  $0\n}".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

fn dynamic_meta_attr_documentation(name: &str) -> Option<Documentation> {
    let text = tfls_core::dynamic_meta_attr_description(name);
    if text.is_empty() {
        return None;
    }
    Some(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: text.to_string(),
    }))
}

fn meta_block_items(kind: BlockKind) -> Vec<CompletionItem> {
    meta_blocks(kind)
        .iter()
        .map(|name| {
            // `provisioner` and `dynamic` take labels; others are plain.
            let snippet = match *name {
                "provisioner" => "provisioner \"${1:local-exec}\" {\n  $0\n}".to_string(),
                "dynamic" => {
                    // dynamic "<label>" { for_each = …; content { … } }
                    // — three tabstops: target-block label, for_each
                    // expression, and the content body.
                    "dynamic \"${1:block_name}\" {\n  for_each = ${2}\n\n  content {\n    $0\n  }\n}".to_string()
                }
                _ => format!("{name} {{\n  $0\n}}"),
            };
            CompletionItem {
                label: (*name).to_string(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some("meta-block".to_string()),
                documentation: meta_block_documentation(name),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect()
}

/// Wrap the canonical meta-arg description in an LSP
/// `Documentation` payload, returning `None` when the description
/// lookup misses so the client doesn't show an empty popup.
fn meta_arg_documentation(name: &str) -> Option<Documentation> {
    let text = tfls_core::meta_attr_description(name);
    if text.is_empty() {
        return None;
    }
    Some(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: text.to_string(),
    }))
}

fn meta_block_documentation(name: &str) -> Option<Documentation> {
    let text = tfls_core::meta_block_description(name);
    if text.is_empty() {
        return None;
    }
    Some(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: text.to_string(),
    }))
}

/// Return the `InsertTextFormat::SNIPPET` body to assign to a
/// provider-schema attribute, shaped around the attribute's
/// declared `type`. Saves the user a keystroke and avoids
/// leaving them typing quotes / brackets by hand:
///
/// - `string`                 → `name = "$1"`
/// - `list(T)` / `set(T)` / `tuple` → `name = [$1]`
/// - `map(T)` / `object(…)`   → `name = {\n  $1\n}` (multi-line; common)
/// - everything else          → `name = $1` (plain; numbers, bools,
///   `any`, dynamic references, etc.)
fn schema_attribute_insert_text(name: &str, attr: &tfls_schema::AttributeSchema) -> String {
    match classify_schema_type(attr.r#type.as_ref()) {
        SchemaTypeKind::String => format!("{name} = \"${{1}}\""),
        SchemaTypeKind::Sequence => format!("{name} = [${{1}}]"),
        SchemaTypeKind::Mapping => format!("{name} = {{\n  ${{1}}\n}}"),
        SchemaTypeKind::Scalar => format!("{name} = ${{1}}"),
    }
}

/// Shape of a provider-declared `cty` type as it appears in
/// `terraform providers schema -json` output. We map the raw JSON
/// down to the four "what bracket do we open?" categories the
/// snippet builder cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaTypeKind {
    String,
    Scalar,   // number, bool, or type we don't want to wrap
    Sequence, // list / set / tuple
    Mapping,  // map / object
}

fn classify_schema_type(ty: Option<&sonic_rs::Value>) -> SchemaTypeKind {
    use sonic_rs::{JsonContainerTrait, JsonValueTrait};
    let Some(ty) = ty else {
        return SchemaTypeKind::Scalar;
    };
    if let Some(s) = ty.as_str() {
        return match s {
            "string" => SchemaTypeKind::String,
            _ => SchemaTypeKind::Scalar,
        };
    }
    // Compound types arrive as `[kind, inner...]` JSON arrays.
    if let Some(arr) = ty.as_array() {
        if let Some(first) = arr.iter().next().and_then(|v| v.as_str()) {
            return match first {
                "list" | "set" | "tuple" => SchemaTypeKind::Sequence,
                "map" | "object" => SchemaTypeKind::Mapping,
                _ => SchemaTypeKind::Scalar,
            };
        }
    }
    SchemaTypeKind::Scalar
}

/// `VariableType` counterpart of [`schema_attribute_insert_text`] —
/// used for module-input completion where we have the child
/// module's parsed `variable "…" { type = … }` but not a provider
/// schema.
fn variable_insert_text(name: &str, ty: Option<&VariableType>) -> String {
    match ty {
        Some(VariableType::Primitive(tfls_core::Primitive::String)) => {
            format!("{name} = \"${{1}}\"")
        }
        Some(VariableType::List(_)) | Some(VariableType::Set(_)) | Some(VariableType::Tuple(_)) => {
            format!("{name} = [${{1}}]")
        }
        Some(VariableType::Map(_)) | Some(VariableType::Object(_)) => {
            format!("{name} = {{\n  ${{1}}\n}}")
        }
        _ => format!("{name} = ${{1}}"),
    }
}

/// `Some(vec![DEPRECATED])` when the item is deprecated, else `None`.
/// Clients render the DEPRECATED tag as a strikethrough on the label.
fn deprecated_tags(deprecated: bool) -> Option<Vec<CompletionItemTag>> {
    deprecated.then(|| vec![CompletionItemTag::DEPRECATED])
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

/// Comprehension / conditional scaffolds offered at expression-start
/// positions. These mirror the constructs users reach for constantly
/// (`[for …]`, `{ for … }`, `cond ? a : b`) but which no function-name
/// completion can supply. Sorted ahead of functions via `00_`.
fn expression_scaffold_items() -> Vec<CompletionItem> {
    const SNIPPETS: &[(&str, &str, &str)] = &[
        (
            "for (list comprehension)",
            "[for ${1:item} in ${2:list} : ${3:item}]",
            "list comprehension",
        ),
        (
            "for (map comprehension)",
            "{ for ${1:k}, ${2:v} in ${3:map} : ${1:k} => ${2:v} }",
            "map comprehension",
        ),
        (
            "ternary (conditional)",
            "${1:condition} ? ${2:true_val} : ${3:false_val}",
            "conditional expression",
        ),
    ];
    SNIPPETS
        .iter()
        .enumerate()
        .map(|(i, (label, snippet, detail))| CompletionItem {
            label: (*label).to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some((*detail).to_string()),
            sort_text: Some(format!("00_{i}")),
            insert_text: Some((*snippet).to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect()
}

/// The enclosing top-level addressable block (`resource` / `data` /
/// `module`) whose span contains `offset`, as `(kind, type, name)`.
/// `kind` is the block ident; for `module` the type slot holds the
/// module name and `name` is empty.
fn enclosing_addressable_block(body: &Body, offset: usize) -> Option<(String, String, String)> {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if !span_contains_offset(block.span(), offset) {
            continue;
        }
        let ident = block.ident.as_str();
        let labels: Vec<String> = block.labels.iter().map(block_label_text).collect();
        return match ident {
            "resource" | "data" => {
                let ty = labels.first()?.clone();
                let name = labels.get(1).cloned().unwrap_or_default();
                Some((ident.to_string(), ty, name))
            }
            "module" => {
                let name = labels.first()?.clone();
                Some(("module".to_string(), name, String::new()))
            }
            _ => None,
        };
    }
    None
}

/// `each.value.<path>` — drill into the for_each element shape of the
/// enclosing block and offer the object fields at `path`.
fn each_attr_items(
    backend: &Backend,
    uri: &Url,
    body: Option<&Body>,
    offset: usize,
    path: &[String],
) -> Vec<CompletionItem> {
    let Some((kind, ty, name)) = body.and_then(|b| enclosing_addressable_block(b, offset)) else {
        return Vec::new();
    };
    // The for_each *collection* shape, keyed by the block's address.
    let root = match kind.as_str() {
        "resource" => IndexRootRef::Resource {
            resource_type: ty,
            name,
        },
        "data" => IndexRootRef::DataSource {
            resource_type: ty,
            name,
        },
        "module" => IndexRootRef::Module { module_name: ty },
        _ => return Vec::new(),
    };
    drill_collection_element_fields(backend, uri, &root, path)
}

/// Resolve the collection shape rooted at `root`, unwrap one level to its
/// element type, walk the `.field` chain in `path`, and offer the object
/// fields at the end. Shared by `each.value.<…>` and `for`-binding
/// drill-down — both name an element of a collection.
fn drill_collection_element_fields(
    backend: &Backend,
    uri: &Url,
    root: &IndexRootRef,
    path: &[String],
) -> Vec<CompletionItem> {
    let Some(collection) = shape_for_root(backend, uri, root) else {
        return Vec::new();
    };
    let Some(element) = for_each_element_shape(&collection) else {
        return Vec::new();
    };
    let steps: Vec<PathStep> = path.iter().map(|p| PathStep::Attr(p.clone())).collect();
    let Some(VariableType::Object(fields)) = walk_shape(&element, &steps) else {
        return Vec::new();
    };
    let mut items: Vec<CompletionItem> = fields
        .keys()
        .map(|k| CompletionItem {
            label: k.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some("element field".to_string()),
            insert_text: Some(k.clone()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// One element of a `for_each` collection. The stored shape is the
/// collection itself (an Object keyed by the for_each keys, or a
/// Map/Set/List), so unwrap a single level to the element type. For an
/// Object we merge all per-key value shapes into one representative
/// element.
fn for_each_element_shape(collection: &VariableType) -> Option<VariableType> {
    match collection {
        VariableType::Object(fields) => {
            let mut acc: Option<VariableType> = None;
            for v in fields.values() {
                acc = Some(match acc {
                    Some(prev) => merge_shapes(prev, v.clone()),
                    None => v.clone(),
                });
            }
            acc
        }
        VariableType::Map(value) | VariableType::List(value) | VariableType::Set(value) => {
            Some((**value).clone())
        }
        _ => None,
    }
}

/// `ignore_changes = [ | ]` — the enclosing resource's own attribute
/// names (and nested block names), plus the special `all` keyword.
/// These are bare identifiers, not references or function calls.
fn ignore_changes_items(backend: &Backend, resource_type: &str) -> Vec<CompletionItem> {
    let mut items = resource_attr_items(backend, resource_type, /*data=*/ false);
    items.insert(
        0,
        CompletionItem {
            label: "all".to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some("ignore changes to every attribute".to_string()),
            sort_text: Some("0_all".to_string()),
            insert_text: Some("all".to_string()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        },
    );
    items
}

/// `depends_on = [ | ]` / `replace_triggered_by = [ | ]` — addresses of
/// every resource, data source, and module declared in the module.
fn depends_on_address_items(backend: &Backend, uri: &Url) -> Vec<CompletionItem> {
    let dir = parent_dir(uri);
    let mut addrs: BTreeSet<String> = BTreeSet::new();
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), dir.as_deref()) {
            continue;
        }
        let table = &entry.value().symbols;
        for addr in table.resources.keys() {
            addrs.insert(format!("{}.{}", addr.resource_type, addr.name));
        }
        for addr in table.data_sources.keys() {
            addrs.insert(format!("data.{}.{}", addr.resource_type, addr.name));
        }
        for name in table.modules.keys() {
            addrs.insert(format!("module.{name}"));
        }
    }
    addrs
        .into_iter()
        .map(|addr| CompletionItem {
            label: addr.clone(),
            kind: Some(CompletionItemKind::REFERENCE),
            detail: Some("address".to_string()),
            insert_text: Some(addr),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect()
}

/// Reference-root completion for a bare expression value in a block with
/// no schema-typed attribute (output `value`, `locals`, module inputs).
/// Offers comprehension scaffolds, the declared `var.` / `local.` /
/// `module.` names, the namespace roots (`each`, `path`, `self`,
/// `data`, `terraform`), and the function list — everything that can
/// legally start an expression there.
fn expression_root_items(backend: &Backend, uri: &Url) -> Vec<CompletionItem> {
    let mut items = expression_scaffold_items();
    let mut sort_index = 0u32;

    let mut push_ref = |label: String, detail: &str, kind: CompletionItemKind, idx: &mut u32| {
        items.push(CompletionItem {
            sort_text: Some(format!("1{:04}_{label}", *idx)),
            label,
            kind: Some(kind),
            detail: Some(detail.to_string()),
            ..Default::default()
        });
        *idx += 1;
    };

    for name in backend.state.all_variable_names() {
        push_ref(
            format!("var.{name}"),
            "variable",
            CompletionItemKind::VARIABLE,
            &mut sort_index,
        );
    }
    for name in backend.state.all_local_names() {
        push_ref(
            format!("local.{name}"),
            "local",
            CompletionItemKind::VARIABLE,
            &mut sort_index,
        );
    }
    for name in module_names_in_scope(backend, uri) {
        push_ref(
            format!("module.{name}"),
            "module",
            CompletionItemKind::MODULE,
            &mut sort_index,
        );
    }

    // Namespace root keywords — insert the root with a trailing dot so the
    // follow-up keystroke re-triggers the namespaced completion.
    for (root, detail) in [
        ("each", "for_each iteration namespace"),
        ("count", "count.index namespace"),
        ("path", "path.module / path.root / path.cwd"),
        ("self", "current resource (inside provisioners)"),
        ("terraform", "terraform.workspace namespace"),
    ] {
        items.push(CompletionItem {
            label: root.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some(detail.to_string()),
            sort_text: Some(format!("2_{root}")),
            insert_text: Some(format!("{root}.")),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        });
    }

    // Functions last (trailing bucket), alphabetical among themselves.
    items.extend(function_name_items(backend).into_iter().map(|mut item| {
        item.sort_text = Some(format!("9_{}", item.label));
        item
    }));

    items
}

fn function_name_items(backend: &Backend) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = backend
        .state
        .functions
        .iter()
        // Skip provider-defined functions — their key is the registry-
        // namespaced form (`provider::hashicorp::aws::arn_parse`), which
        // isn't valid HCL as a plain call. They're surfaced correctly via
        // the ProviderFunctionNamespace / ProviderFunctionName contexts.
        .filter(|entry| !entry.key().starts_with("provider::"))
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

/// Provider-defined function completion (Terraform 1.8+):
/// `provider::|` → set of distinct local provider names that have
/// at least one function. Functions are stored in
/// `state.functions` under their fully-qualified name
/// `provider::<ns>::<name>::<fn>`; we project out the third
/// segment (`<name>`) and remap it through the doc's
/// `required_providers { LOCAL = { source = "ns/name" } }` block
/// so renamed providers (`aws_v6 = { source = "hashicorp/aws" }`)
/// surface under the user-visible local name `aws_v6`, not the
/// provider's underlying name `aws`.
fn provider_function_namespace_items(backend: &Backend, uri: &Url) -> Vec<CompletionItem> {
    use std::collections::{BTreeSet, HashMap};
    // Reverse map: provider-name → local-name. Built once per call;
    // skipped if the doc has no `required_providers` block (every
    // local is then assumed to equal its provider name).
    let provider_to_local: HashMap<String, String> = name_to_local_map(backend, uri);
    let mut locals: BTreeSet<String> = BTreeSet::new();
    for entry in backend.state.functions.iter() {
        let qualified = entry.key();
        let Some(provider_name) = qualified_function_local_name(qualified) else {
            continue;
        };
        let local = provider_to_local
            .get(provider_name)
            .cloned()
            .unwrap_or_else(|| provider_name.to_string());
        locals.insert(local);
    }
    locals
        .into_iter()
        .map(|local| CompletionItem {
            label: local.clone(),
            kind: Some(CompletionItemKind::MODULE),
            detail: Some("provider".to_string()),
            insert_text: Some(format!("{local}::")),
            ..Default::default()
        })
        .collect()
}

/// `provider::<local>::|` → function names exposed by that provider.
/// Resolve `<local>` through `required_providers` to the actual
/// provider name (default: identity), then filter the function map
/// by third segment.
fn provider_function_name_items(
    backend: &Backend,
    uri: &Url,
    provider_local: &str,
) -> Vec<CompletionItem> {
    let provider_name = local_to_provider_name(backend, uri, provider_local);
    let mut items: Vec<CompletionItem> = backend
        .state
        .functions
        .iter()
        .filter_map(|entry| {
            let qualified = entry.key();
            let local = qualified_function_local_name(qualified)?;
            if local != provider_name {
                return None;
            }
            let fn_name = qualified_function_short_name(qualified)?.to_string();
            let sig = entry.value();
            Some(CompletionItem {
                label: fn_name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(sig.label(qualified)),
                documentation: sig.description.as_ref().map(|d| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: d.clone(),
                    })
                }),
                insert_text: Some(format!("{fn_name}(${{1}})$0")),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            })
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Resolve a local provider name (`aws_v6`) to the underlying
/// provider name (`aws`) by consulting `terraform { required_providers
/// { LOCAL = { source = "ns/name" } } }`. Falls back to the local name
/// itself when no override is present.
///
/// Walks the active doc first, then every peer `.tf` doc in the same
/// directory — `required_providers` typically lives in `versions.tf`
/// while the user is editing a different file. The active doc may
/// also have a parse error mid-edit (cursor in the middle of an
/// unfinished `provider::LOCAL::` expression), in which case the
/// peer walk is the ONLY way to resolve.
fn local_to_provider_name(backend: &Backend, uri: &Url, local: &str) -> String {
    if let Some(doc) = backend.state.documents.get(uri) {
        if let Some(body) = doc.parsed.body.as_ref() {
            if let Some(name) = required_providers_local_to_name(body, local) {
                return name;
            }
        }
    }
    if let Some(target_dir) = super::util::parent_dir(uri) {
        for entry in backend.state.documents.iter() {
            let other_uri = entry.key();
            if other_uri == uri {
                continue;
            }
            let Ok(path) = other_uri.to_file_path() else {
                continue;
            };
            if path.parent() != Some(target_dir.as_path()) {
                continue;
            }
            let doc = entry.value();
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            if let Some(name) = required_providers_local_to_name(body, local) {
                return name;
            }
        }
    }
    local.to_string()
}

/// Build a reverse `provider-name → local-name` map from the doc's
/// `required_providers` (and peer `.tf` files in the same dir, since
/// `required_providers` typically lives in `versions.tf`). Entries
/// without an explicit `source` map `LOCAL → LOCAL`.
fn name_to_local_map(backend: &Backend, uri: &Url) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;
    let mut out: HashMap<String, String> = HashMap::new();
    if let Some(doc) = backend.state.documents.get(uri) {
        if let Some(body) = doc.parsed.body.as_ref() {
            for (k, v) in required_providers_name_to_local(body) {
                out.entry(k).or_insert(v);
            }
        }
    }
    if let Some(target_dir) = super::util::parent_dir(uri) {
        for entry in backend.state.documents.iter() {
            let other_uri = entry.key();
            if other_uri == uri {
                continue;
            }
            let Ok(path) = other_uri.to_file_path() else {
                continue;
            };
            if path.parent() != Some(target_dir.as_path()) {
                continue;
            }
            let doc = entry.value();
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            for (k, v) in required_providers_name_to_local(body) {
                out.entry(k).or_insert(v);
            }
        }
    }
    out
}

/// Pub re-export so handlers in sibling modules (signature_help)
/// can resolve a local provider name without duplicating the body
/// walk.
pub fn required_providers_local_to_name_pub(
    body: &hcl_edit::structure::Body,
    local: &str,
) -> Option<String> {
    required_providers_local_to_name(body, local)
}

/// Walk `terraform { required_providers { ... } }` and return the
/// provider name for `local`. Long form `LOCAL = { source = "ns/name" }`
/// returns `name`; short form `LOCAL = "version"` returns
/// `LOCAL` (HashiCorp registry default).
fn required_providers_local_to_name(
    body: &hcl_edit::structure::Body,
    local: &str,
) -> Option<String> {
    use hcl_edit::expr::Expression;
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "terraform" {
            continue;
        }
        for inner in block.body.iter() {
            let Some(rp_block) = inner.as_block() else {
                continue;
            };
            if rp_block.ident.as_str() != "required_providers" {
                continue;
            }
            for entry in rp_block.body.iter() {
                let Some(attr) = entry.as_attribute() else {
                    continue;
                };
                if attr.key.as_str() != local {
                    continue;
                }
                // Long form: `LOCAL = { source = "...", ... }`.
                if let Expression::Object(obj) = &attr.value {
                    for (key, value) in obj.iter() {
                        if let Some(k) = object_key_as_str(key) {
                            if k == "source" {
                                if let Some(s) = expr_literal_string(value.expr()) {
                                    return parse_source_provider_name(&s)
                                        .or_else(|| Some(local.to_string()));
                                }
                            }
                        }
                    }
                }
                // Short form (`LOCAL = "~> X"`) or missing source —
                // the provider name is the local name.
                return Some(local.to_string());
            }
        }
    }
    None
}

/// Walk `terraform { required_providers { ... } }` and return a map
/// `provider-name → local-name`.
fn required_providers_name_to_local(
    body: &hcl_edit::structure::Body,
) -> std::collections::HashMap<String, String> {
    use hcl_edit::expr::Expression;
    use std::collections::HashMap;
    let mut out = HashMap::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "terraform" {
            continue;
        }
        for inner in block.body.iter() {
            let Some(rp_block) = inner.as_block() else {
                continue;
            };
            if rp_block.ident.as_str() != "required_providers" {
                continue;
            }
            for entry in rp_block.body.iter() {
                let Some(attr) = entry.as_attribute() else {
                    continue;
                };
                let local = attr.key.as_str().to_string();
                let mut name: Option<String> = None;
                if let Expression::Object(obj) = &attr.value {
                    for (key, value) in obj.iter() {
                        if let Some(k) = object_key_as_str(key) {
                            if k == "source" {
                                if let Some(s) = expr_literal_string(value.expr()) {
                                    name = parse_source_provider_name(&s);
                                }
                            }
                        }
                    }
                }
                let resolved = name.unwrap_or_else(|| local.clone());
                out.insert(resolved, local);
            }
        }
    }
    out
}

fn object_key_as_str(key: &hcl_edit::expr::ObjectKey) -> Option<&str> {
    use hcl_edit::expr::ObjectKey;
    match key {
        ObjectKey::Ident(i) => Some(i.as_str()),
        ObjectKey::Expression(hcl_edit::expr::Expression::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn expr_literal_string(expr: &hcl_edit::expr::Expression) -> Option<String> {
    use hcl_edit::expr::Expression;
    match expr {
        Expression::String(s) => Some(s.as_str().to_string()),
        Expression::StringTemplate(t) => {
            let mut collected = String::new();
            for element in t.iter() {
                match element {
                    hcl_edit::template::Element::Literal(lit) => collected.push_str(lit.as_str()),
                    _ => return None,
                }
            }
            Some(collected)
        }
        _ => None,
    }
}

/// Extract the provider name from a `required_providers` source
/// string. Accepts both short (`hashicorp/aws`) and long
/// (`registry.terraform.io/hashicorp/aws`) forms; the trailing
/// segment is always the provider name.
fn parse_source_provider_name(src: &str) -> Option<String> {
    let last = src.rsplit('/').next()?;
    if last.is_empty() {
        return None;
    }
    Some(last.to_string())
}

/// Project the local-name segment out of a qualified function key
/// `provider::<ns>::<name>::<fn>`. Returns `None` for keys that
/// don't fit the expected four-segment shape.
fn qualified_function_local_name(qualified: &str) -> Option<&str> {
    let mut parts = qualified.split("::");
    if parts.next()? != "provider" {
        return None;
    }
    parts.next()?;
    let local = parts.next()?;
    let _fn = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(local)
}

/// Project the function-name segment out of a qualified function
/// key `provider::<ns>::<name>::<fn>`.
fn qualified_function_short_name(qualified: &str) -> Option<&str> {
    let mut parts = qualified.split("::");
    if parts.next()? != "provider" {
        return None;
    }
    parts.next()?;
    parts.next()?;
    let fn_name = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(fn_name)
}

/// Suggest the registry-mined enum values for a top-level
/// resource / data-source attribute. Strings get wrapped in
/// quotes; numeric / boolean literals are emitted bare so the
/// completion produces valid HCL either way. Callers prepend
/// these so they sort above generic var.* / local.* /
/// function suggestions.
fn allowed_value_items(
    backend: &Backend,
    resource_type: &str,
    attr_name: &str,
    sort_index: &mut u32,
) -> Vec<CompletionItem> {
    let schema = backend
        .state
        .resource_schema(resource_type)
        .or_else(|| backend.state.data_source_schema(resource_type));
    let Some(schema) = schema else {
        return Vec::new();
    };
    let Some(attr) = schema.block.attributes.get(attr_name) else {
        return Vec::new();
    };
    let Some(values) = attr.allowed_values.as_deref() else {
        return Vec::new();
    };
    if values.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<CompletionItem> = Vec::with_capacity(values.len());
    for v in values {
        let insert_text = if literal_should_be_unquoted(v) {
            v.clone()
        } else {
            format!("\"{v}\"")
        };
        out.push(CompletionItem {
            label: insert_text.clone(),
            kind: Some(CompletionItemKind::ENUM_MEMBER),
            detail: Some("valid value".to_string()),
            insert_text: Some(insert_text.clone()),
            sort_text: Some(format!("{:04}_{insert_text}", *sort_index)),
            ..Default::default()
        });
        *sort_index += 1;
    }
    out
}

/// True when `v` is a number or boolean literal — i.e. should
/// land in the buffer as `42` / `true` rather than `"42"` /
/// `"true"`. Plugin Framework's stringified-boolean attributes
/// (e.g. azurerm `log_progress`) intentionally take the QUOTED
/// form, so the heuristic only emits a bare literal when the
/// value parses cleanly as a Rust number / bool.
fn literal_should_be_unquoted(v: &str) -> bool {
    if v.parse::<i64>().is_ok() || v.parse::<f64>().is_ok() {
        return true;
    }
    // Note: NOT matching "true" / "false" here — many string
    // attrs in Plugin Framework providers accept ONLY the quoted
    // form (azurerm log_progress takes "true" not true). Erring
    // on the side of quoting is the safer default; a user who
    // wants unquoted can delete the quotes.
    false
}

/// Context-aware value completions: suggest matching resources, data
/// sources, variables, locals, and functions based on the attribute
/// being edited.
fn attribute_value_items(
    backend: &Backend,
    resource_type: &str,
    attr_name: &str,
) -> Vec<CompletionItem> {
    use super::attr_ref_map;
    use std::collections::HashSet;

    let mut items = Vec::new();
    let mut sort_index = 0u32;

    // Enum values mined from the registry's hand-written Markdown
    // (see `tfls_provider_protocol::registry_docs::extract_allowed_values`).
    // Emit these FIRST — they're the most concrete suggestion the
    // server can make, and Plugin Framework providers don't expose
    // validators over the wire so the schema's bare `string` type
    // is otherwise the only signal.
    items.extend(allowed_value_items(
        backend,
        resource_type,
        attr_name,
        &mut sort_index,
    ));

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
    // function_name_items carries no sort_text, so the refs above (which
    // do) would interleave with alphabetical function labels once a client
    // falls back to label sort. Re-stamp the functions into a trailing
    // bucket so refs stay first and functions remain the fallback tail.
    items.extend(function_name_items(backend).into_iter().map(|mut item| {
        item.sort_text = Some(format!("9{sort_index:04}_{}", item.label));
        item
    }));

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
    Outputs,
}

/// `each.<...>` namespace completion. Surfaces inside any
/// resource/data/module body that uses `for_each`. Two fixed
/// fields per Terraform spec.
fn each_namespace_items() -> Vec<CompletionItem> {
    vec![
        builtin_namespace_item(
            "key",
            "The map key (or set member) of the current `for_each` iteration. \
             Always a string.",
        ),
        builtin_namespace_item(
            "value",
            "The value associated with the current `for_each` iteration's key. \
             Type depends on the `for_each` collection.",
        ),
    ]
}

/// `count.<...>` namespace completion. Surfaces inside any
/// resource/data/module body that uses `count`. One fixed field.
fn count_namespace_items() -> Vec<CompletionItem> {
    vec![builtin_namespace_item(
        "index",
        "The 0-based index of the current `count`-iterated instance.",
    )]
}

/// `path.<...>` namespace completion. Three fixed filesystem-path
/// accessors Terraform exposes globally.
fn path_namespace_items() -> Vec<CompletionItem> {
    vec![
        builtin_namespace_item(
            "module",
            "Filesystem path of the module where the expression is placed.",
        ),
        builtin_namespace_item(
            "root",
            "Filesystem path of the root module of the configuration.",
        ),
        builtin_namespace_item(
            "cwd",
            "Filesystem path of the directory where Terraform was invoked.",
        ),
    ]
}

/// `terraform.<...>` namespace completion. The only field is
/// `workspace`.
fn terraform_namespace_items() -> Vec<CompletionItem> {
    vec![builtin_namespace_item(
        "workspace",
        "Name of the currently active Terraform workspace.",
    )]
}

fn builtin_namespace_item(label: &str, doc: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::PROPERTY),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: doc.to_string(),
        })),
        ..Default::default()
    }
}

/// True for `*.tfvars` / `*.tfvars.json` variable-definition files.
fn is_tfvars_uri(uri: &Url) -> bool {
    let path = uri.path();
    path.ends_with(".tfvars") || path.ends_with(".tfvars.json")
}

/// Completion for a `.tfvars` file. At a key position (no `=` yet on the
/// line) we offer the module's declared variable names that aren't
/// already assigned, inserting `name = ${1}`. Value positions (after
/// `=`) yield nothing — the value is a literal we can't reliably
/// suggest without type-shape inference.
fn tfvars_completion_items(
    backend: &Backend,
    uri: &Url,
    body: Option<&Body>,
    line_prefix: &str,
) -> Vec<CompletionItem> {
    use std::collections::HashSet;
    // A `=` earlier on the line means the cursor is in value position.
    if line_prefix.contains('=') {
        return Vec::new();
    }
    // Keys already assigned in this file — don't re-offer them.
    let assigned: HashSet<String> = body
        .map(|b| {
            b.iter()
                .filter_map(|s| s.as_attribute().map(|a| a.key.as_str().to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Declared variables across the sibling `.tf` files in this dir.
    let dir = parent_dir(uri);
    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), dir.as_deref()) {
            continue;
        }
        for n in entry.value().symbols.variables.keys() {
            names.insert(n.clone());
        }
    }
    names
        .into_iter()
        .filter(|n| !assigned.contains(n))
        .map(|n| CompletionItem {
            label: n.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some("variable".to_string()),
            insert_text: Some(format!("{n} = ${{1}}")),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect()
}

/// Top-level block snippets for a test file. Mirrors
/// [`TOP_LEVEL_SNIPPETS`] but over the closed `.tftest.hcl` grammar — so
/// `resource` / `data` never appear and `run` is offered instead.
const TFTEST_TOP_LEVEL_SNIPPETS: &[(&str, &str, &str)] = &[
    (
        "run",
        "run \"${1:name}\" {\n  command = ${2:plan}\n  $0\n}",
        "Test run (a plan or apply step)",
    ),
    (
        "variables",
        "variables {\n  $0\n}",
        "Input-variable values shared across runs",
    ),
    (
        "provider",
        "provider \"${1:name}\" {\n  $0\n}",
        "Provider configuration for the test",
    ),
    (
        "mock_provider",
        "mock_provider \"${1:name}\" {\n  $0\n}",
        "Replace a provider with mock data",
    ),
    (
        "override_resource",
        "override_resource {\n  target = ${1}\n  values = {\n    $0\n  }\n}",
        "Override a resource's computed values",
    ),
    (
        "override_data",
        "override_data {\n  target = ${1}\n  values = {\n    $0\n  }\n}",
        "Override a data source's result",
    ),
    (
        "override_module",
        "override_module {\n  target  = ${1}\n  outputs = {\n    $0\n  }\n}",
        "Override a child module's outputs",
    ),
    (
        "test",
        "test {\n  parallel = ${1:true}\n}",
        "Per-file test options",
    ),
];

/// Completion for a `.tftest.hcl` / `.tftest.json` test file. Routes by
/// cursor context over the closed test grammar:
///
/// - A reference prefix (`var.`, `output.`, `run.`) → names from the
///   module under test (variables / outputs) or this file's run labels.
/// - Top level → the test-file block snippets (`run`, `mock_provider`, …).
/// - Inside a recognised block → that block's attrs + nested-block
///   snippets, via [`builtin_blocks::tftest_root_schema`].
fn tftest_completion_items(
    backend: &Backend,
    uri: &Url,
    body: Option<&Body>,
    line_prefix: &str,
    offset: usize,
) -> Vec<CompletionItem> {
    // 1. Reference-value completion (`var.` / `output.` / `run.`).
    if let Some(items) = tftest_reference_items(backend, uri, body, line_prefix) {
        return items;
    }

    // 2. Block / attribute completion keyed on the enclosing block path.
    let path = body
        .map(|b| tftest_block_path(b, offset))
        .unwrap_or_default();
    if path.is_empty() {
        return tftest_top_level_items();
    }
    let Some(schema) = tftest_resolve_schema(&path) else {
        // Freeform body (`variables {}`, `provider {}`, `mock_resource`
        // `defaults`) — nothing schema-driven to offer.
        return Vec::new();
    };
    let filter = compute_body_filter(body, offset);
    builtin_body_items(schema, &filter)
}

fn tftest_top_level_items() -> Vec<CompletionItem> {
    TFTEST_TOP_LEVEL_SNIPPETS
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

/// Full enclosing-block keyword path at `offset`, INCLUDING the top-level
/// block (unlike [`nested_block_path`], whose caller already knows the
/// root). Reuses `collect_nested_path` for the inner descent.
fn tftest_block_path(body: &Body, offset: usize) -> Vec<String> {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if span_contains_offset(block.span(), offset) && cursor_in_block_body(block, offset) {
            let mut path = vec![block.ident.as_str().to_string()];
            collect_nested_path(&block.body, offset, &mut path);
            return path;
        }
    }
    Vec::new()
}

/// Resolve a test-file block path to the [`BuiltinSchema`] of the body the
/// cursor is in. Starts at [`builtin_blocks::tftest_root_schema`] and
/// descends nested blocks; `None` for an unknown or freeform body.
fn tftest_resolve_schema(path: &[String]) -> Option<builtin_blocks::BuiltinSchema> {
    let mut iter = path.iter();
    let root = iter.next()?;
    let mut schema = builtin_blocks::tftest_root_schema(root)?;
    for step in iter {
        let block = schema.blocks.iter().find(|b| b.name == step.as_str())?;
        schema = block.body_schema()?;
    }
    Some(schema)
}

/// `var.` / `output.` / `run.` reference-value completion in a test file.
/// Returns `None` when the cursor isn't in such a reference position so
/// the caller falls through to block/attr completion.
fn tftest_reference_items(
    backend: &Backend,
    uri: &Url,
    body: Option<&Body>,
    line_prefix: &str,
) -> Option<Vec<CompletionItem>> {
    let (root, _partial) = tftest_reference_prefix(line_prefix)?;
    let items = match root {
        "var" => tftest_mut_symbol_items(backend, uri, SymbolField::Variables),
        "output" => tftest_mut_symbol_items(backend, uri, SymbolField::Outputs),
        "run" => tftest_run_label_items(body),
        _ => return None,
    };
    Some(items)
}

/// If the cursor sits in a `<root>.<partial>` reference for a test-file
/// namespace (`var` / `output` / `run`), return `(root, partial)`. The
/// `partial` is whatever's been typed after the dot (may be empty).
fn tftest_reference_prefix(line_prefix: &str) -> Option<(&'static str, &str)> {
    // Walk back over the trailing identifier-ish run (letters, digits,
    // `_`, `-`, `.`) to isolate the reference token under the cursor.
    // `rfind` returns the BYTE offset of the delimiter char's first byte;
    // skip past the WHOLE char (it may be multibyte — an em-dash, NBSP,
    // smart quote) so the slice lands on a UTF-8 boundary rather than
    // mid-codepoint (which would panic the completion request).
    let token_start = line_prefix
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'))
        .map(|i| i + line_prefix[i..].chars().next().map_or(1, char::len_utf8))
        .unwrap_or(0);
    let token = line_prefix.get(token_start..).unwrap_or("");
    for root in ["var", "output", "run"] {
        if let Some(rest) = token.strip_prefix(root) {
            if let Some(partial) = rest.strip_prefix('.') {
                // Only a single segment after the root (no further dot) —
                // `run.x.out` value completion isn't offered here.
                if !partial.contains('.') {
                    return Some((root, partial));
                }
            }
        }
    }
    None
}

/// Variable / output names declared in the module under test.
fn tftest_mut_symbol_items(
    backend: &Backend,
    uri: &Url,
    field: SymbolField,
) -> Vec<CompletionItem> {
    let dir = super::util::module_under_test_dir(uri);
    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), dir.as_deref()) {
            continue;
        }
        let table = &entry.value().symbols;
        let keys: Box<dyn Iterator<Item = &String>> = match field {
            SymbolField::Variables => Box::new(table.variables.keys()),
            SymbolField::Outputs => Box::new(table.outputs.keys()),
            _ => Box::new(std::iter::empty()),
        };
        for n in keys {
            names.insert(n.clone());
        }
    }
    let detail = match field {
        SymbolField::Outputs => "module output",
        _ => "module variable",
    };
    names
        .into_iter()
        .map(|n| CompletionItem {
            label: n.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(detail.to_string()),
            insert_text: Some(n),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect()
}

/// `run "<label>"` block labels declared in this test file (the targets
/// of `run.<label>.<output>` references).
fn tftest_run_label_items(body: Option<&Body>) -> Vec<CompletionItem> {
    let Some(body) = body else {
        return Vec::new();
    };
    let mut labels: BTreeSet<String> = BTreeSet::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "run" {
            continue;
        }
        if let Some(label) = block.labels.first() {
            labels.insert(block_label_text(label));
        }
    }
    labels
        .into_iter()
        .map(|n| CompletionItem {
            label: n.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some("run block".to_string()),
            insert_text: Some(n),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        })
        .collect()
}

/// Sorted, de-duplicated declared `module "<name>"` block names across
/// the current directory.
fn module_names_in_scope(backend: &Backend, uri: &Url) -> Vec<String> {
    let dir = parent_dir(uri);
    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in backend.state.documents.iter() {
        if !doc_in_dir(entry.key(), dir.as_deref()) {
            continue;
        }
        for n in entry.value().symbols.modules.keys() {
            names.insert(n.clone());
        }
    }
    names.into_iter().collect()
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
            SymbolField::Outputs => {
                for n in doc.symbols.outputs.keys() {
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

    // Nested blocks (root_block_device, network_interface, …) are valid
    // referenceable targets — `aws_instance.x.root_block_device` resolves
    // to a list/object of the block's attributes — yet only the flat
    // attribute set was offered. Surface each nested block name too.
    // Skip config-only blocks that aren't reference targets.
    const CONFIG_ONLY_BLOCKS: &[&str] = &["timeouts", "lifecycle"];
    for name in schema.block.block_types.keys() {
        if CONFIG_ONLY_BLOCKS.contains(&name.as_str()) {
            continue;
        }
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::STRUCT),
            detail: Some("nested block".to_string()),
            insert_text: Some(name.clone()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        });
    }
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
        Some(d) => match parent_dir(doc_uri) {
            Some(p) => super::util::dir_paths_match(&p, d),
            None => false,
        },
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
            let var_type = table.variable_types.get(name);
            let type_detail = var_type.map(|t| format!("{t}"));
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
                insert_text: Some(variable_insert_text(name, var_type)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Output completions for `module.NAME.|`.
fn module_output_items(backend: &Backend, uri: &Url, module_name: &str) -> Vec<CompletionItem> {
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
fn shape_for_root(backend: &Backend, uri: &Url, root: &IndexRootRef) -> Option<VariableType> {
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
            | (VariableType::Object(fields), PathStep::Attr(k)) => fields.get(k).cloned()?,
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
/// Attach a `text_edit` to version-completion items so accepting one
/// REPLACES the partially-typed version token instead of inserting after
/// it. Without this, typing `version = "5.9|"` and accepting `5.94.0`
/// yields `5.95.94.0` on clients that don't strip the typed prefix.
/// Only the `InsideVersion` slot (the user is typing version digits) needs
/// a replace; at/after an operator the items append at the cursor.
fn stamp_version_replace(
    items: Vec<CompletionItem>,
    pos: Position,
    cursor_partial: &str,
) -> Vec<CompletionItem> {
    use tfls_core::version_constraint::{cursor_slot, CursorSlot};
    let partial_len = match cursor_slot(cursor_partial, cursor_partial.len()) {
        // Version tokens are ASCII, so char count == UTF-16 CU count.
        CursorSlot::InsideVersion { partial, .. } => partial.chars().count() as u32,
        _ => 0,
    };
    if partial_len == 0 {
        return items;
    }
    let range = Range {
        start: Position::new(pos.line, pos.character.saturating_sub(partial_len)),
        end: pos,
    };
    items
        .into_iter()
        .map(|mut it| {
            let new_text = it.insert_text.take().unwrap_or_else(|| it.label.clone());
            if it.filter_text.is_none() {
                it.filter_text = Some(it.label.clone());
            }
            it.text_edit = Some(CompletionTextEdit::Edit(TextEdit { range, new_text }));
            it
        })
        .collect()
}

fn compute_index_replace_range(line: &str, pos: Position) -> Range {
    // `pos.character` is a UTF-16 code-unit column; convert to a byte
    // index for the slice/scan math, then convert the resulting byte
    // positions back to UTF-16 columns for the emitted Range. Treating it
    // as a byte index misaligns the replace span on lines with multibyte
    // text before the `[` and overwrites the wrong region on accept.
    let byte_col = utf16_cu_to_byte(line, pos.character as usize);
    let before = &line[..byte_col];
    let bracket_byte = before.rfind('[').map(|b| b + 1).unwrap_or(byte_col);

    let after = &line[byte_col..];
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
        start: Position::new(pos.line, byte_to_utf16_cu(line, bracket_byte) as u32),
        end: Position::new(pos.line, byte_to_utf16_cu(line, byte_col + consumed) as u32),
    }
}

/// Byte offset of the UTF-16 code-unit column `cu` within `line`
/// (clamped to the line's end). Lands on a char boundary.
fn utf16_cu_to_byte(line: &str, cu: usize) -> usize {
    let mut acc = 0usize;
    for (b, c) in line.char_indices() {
        if acc >= cu {
            return b;
        }
        acc += c.len_utf16();
    }
    line.len()
}

/// UTF-16 code-unit column of byte offset `byte` within `line`.
fn byte_to_utf16_cu(line: &str, byte: usize) -> usize {
    let mut acc = 0usize;
    for (b, c) in line.char_indices() {
        if b >= byte {
            break;
        }
        acc += c.len_utf16();
    }
    acc
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod stamp_version_replace_tests {
    use super::*;

    fn item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: Some(label.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn inside_version_replaces_typed_partial() {
        // `version = "5.9|"` — cursor at column 14, partial "5.9".
        let out = stamp_version_replace(vec![item("5.94.0")], Position::new(0, 14), "5.9");
        let te = match &out[0].text_edit {
            Some(CompletionTextEdit::Edit(e)) => e,
            other => panic!("expected text_edit, got {other:?}"),
        };
        assert_eq!((te.range.start.character, te.range.end.character), (11, 14));
        assert_eq!(te.new_text, "5.94.0");
        assert!(
            out[0].insert_text.is_none(),
            "insert_text cleared in favour of text_edit"
        );
    }

    #[test]
    fn after_operator_does_not_set_text_edit() {
        // `version = ">= |"` — nothing to replace; append at cursor.
        let out = stamp_version_replace(vec![item("5.94.0")], Position::new(0, 14), ">= ");
        assert!(out[0].text_edit.is_none());
        assert!(out[0].insert_text.is_some());
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

    #[test]
    fn cursor_column_past_end_of_line_does_not_panic() {
        // Client may send a `character` past EOL; clamps to the line's end
        // (column 6 for the 6-char `var.x[`) without panicking.
        let (s, e) = range("var.x[", 20);
        assert_eq!((s, e), (6, 6));
    }

    #[test]
    fn cursor_column_mid_codepoint_does_not_panic() {
        // A multibyte char before the cursor must not panic — `pos.character`
        // is treated as a UTF-16 column, converted to a char-boundary byte.
        let line = "x[\u{00e9}]"; // `x[é]`
        let _ = range(line, 3);
    }

    #[test]
    fn multibyte_before_bracket_yields_utf16_columns() {
        // `var.régions["a"]` — `é` is 1 UTF-16 CU but 2 bytes. Cursor in
        // UTF-16 column 12 (just after the `[`), key `"a"`. The emitted
        // Range must use UTF-16 columns: start at 12 (after `[`), end 16
        // (after `]`), NOT byte columns (which would be off by one).
        let line = "var.régions[\"a\"]";
        let (s, e) = range(line, 12);
        assert_eq!((s, e), (12, 16), "UTF-16 columns, not byte offsets");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod required_providers_resolver_tests {
    use super::*;

    fn parse_body(src: &str) -> hcl_edit::structure::Body {
        src.parse().expect("parse")
    }

    #[test]
    fn resolves_long_form_source() {
        let src = "terraform {\n  required_providers {\n    aws_v6 = {\n      source = \"hashicorp/aws\"\n    }\n  }\n}\n";
        let body = parse_body(src);
        assert_eq!(
            required_providers_local_to_name(&body, "aws_v6"),
            Some("aws".to_string())
        );
    }

    #[test]
    fn resolves_long_form_with_full_hostname() {
        let src = "terraform {\n  required_providers {\n    aws_v6 = {\n      source = \"registry.terraform.io/hashicorp/aws\"\n    }\n  }\n}\n";
        let body = parse_body(src);
        assert_eq!(
            required_providers_local_to_name(&body, "aws_v6"),
            Some("aws".to_string())
        );
    }

    #[test]
    fn falls_back_to_local_for_short_form() {
        let src = "terraform {\n  required_providers {\n    aws = \"~> 4.0\"\n  }\n}\n";
        let body = parse_body(src);
        assert_eq!(
            required_providers_local_to_name(&body, "aws"),
            Some("aws".to_string())
        );
    }

    #[test]
    fn unknown_local_returns_none() {
        let src = "terraform {\n  required_providers {\n    aws = \"~> 4.0\"\n  }\n}\n";
        let body = parse_body(src);
        assert_eq!(required_providers_local_to_name(&body, "kubernetes"), None);
    }

    #[test]
    fn name_to_local_inverts() {
        let src = "terraform {\n  required_providers {\n    aws_v6 = {\n      source = \"hashicorp/aws\"\n    }\n    k8s = {\n      source = \"hashicorp/kubernetes\"\n    }\n  }\n}\n";
        let body = parse_body(src);
        let m = required_providers_name_to_local(&body);
        assert_eq!(m.get("aws"), Some(&"aws_v6".to_string()));
        assert_eq!(m.get("kubernetes"), Some(&"k8s".to_string()));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod type_aware_insert_tests {
    use super::*;
    use tfls_schema::AttributeSchema;

    fn schema_with_type(json: &str) -> AttributeSchema {
        AttributeSchema {
            r#type: Some(sonic_rs::from_str(json).expect("parse type")),
            ..Default::default()
        }
    }

    #[test]
    fn string_wraps_in_quotes() {
        let attr = schema_with_type(r#""string""#);
        assert_eq!(schema_attribute_insert_text("ami", &attr), "ami = \"${1}\"");
    }

    #[test]
    fn number_stays_bare() {
        let attr = schema_with_type(r#""number""#);
        assert_eq!(schema_attribute_insert_text("count", &attr), "count = ${1}");
    }

    #[test]
    fn bool_stays_bare() {
        let attr = schema_with_type(r#""bool""#);
        assert_eq!(
            schema_attribute_insert_text("enabled", &attr),
            "enabled = ${1}"
        );
    }

    #[test]
    fn list_of_string_gets_square_brackets() {
        let attr = schema_with_type(r#"["list", "string"]"#);
        assert_eq!(schema_attribute_insert_text("tags", &attr), "tags = [${1}]");
    }

    #[test]
    fn set_of_string_gets_square_brackets() {
        let attr = schema_with_type(r#"["set", "string"]"#);
        assert_eq!(
            schema_attribute_insert_text("sg_ids", &attr),
            "sg_ids = [${1}]"
        );
    }

    #[test]
    fn tuple_gets_square_brackets() {
        let attr = schema_with_type(r#"["tuple", ["string", "number"]]"#);
        assert_eq!(schema_attribute_insert_text("pair", &attr), "pair = [${1}]");
    }

    #[test]
    fn map_gets_curly_braces_multiline() {
        let attr = schema_with_type(r#"["map", "string"]"#);
        assert_eq!(
            schema_attribute_insert_text("tags", &attr),
            "tags = {\n  ${1}\n}"
        );
    }

    #[test]
    fn object_gets_curly_braces_multiline() {
        let attr = schema_with_type(r#"["object", {"name": "string"}]"#);
        assert_eq!(
            schema_attribute_insert_text("cfg", &attr),
            "cfg = {\n  ${1}\n}"
        );
    }

    #[test]
    fn missing_type_falls_back_to_bare() {
        let attr = AttributeSchema::default();
        assert_eq!(schema_attribute_insert_text("x", &attr), "x = ${1}");
    }

    #[test]
    fn unknown_primitive_falls_back_to_bare() {
        let attr = schema_with_type(r#""dynamic""#);
        assert_eq!(schema_attribute_insert_text("x", &attr), "x = ${1}");
    }

    // --- variable_insert_text (module inputs) ---

    #[test]
    fn variable_string_wraps_in_quotes() {
        let t = VariableType::Primitive(tfls_core::Primitive::String);
        assert_eq!(variable_insert_text("name", Some(&t)), "name = \"${1}\"");
    }

    #[test]
    fn variable_list_gets_square_brackets() {
        let t = VariableType::List(Box::new(VariableType::Primitive(
            tfls_core::Primitive::String,
        )));
        assert_eq!(variable_insert_text("tags", Some(&t)), "tags = [${1}]");
    }

    #[test]
    fn variable_map_gets_curly_braces() {
        let t = VariableType::Map(Box::new(VariableType::Primitive(
            tfls_core::Primitive::String,
        )));
        assert_eq!(
            variable_insert_text("tags", Some(&t)),
            "tags = {\n  ${1}\n}"
        );
    }

    #[test]
    fn variable_any_falls_back_to_bare() {
        let t = VariableType::Any;
        assert_eq!(variable_insert_text("x", Some(&t)), "x = ${1}");
    }

    #[test]
    fn variable_without_type_falls_back_to_bare() {
        assert_eq!(variable_insert_text("x", None), "x = ${1}");
    }

    #[test]
    fn literal_should_be_unquoted_emits_bare_for_numbers() {
        assert!(literal_should_be_unquoted("0"));
        assert!(literal_should_be_unquoted("9"));
        assert!(literal_should_be_unquoted("3.14"));
    }

    #[test]
    fn literal_should_be_unquoted_emits_quoted_for_strings_and_bools() {
        // Plugin Framework stringified-bool attributes take the
        // quoted form even though the value LOOKS like a bool.
        assert!(!literal_should_be_unquoted("true"));
        assert!(!literal_should_be_unquoted("false"));
        assert!(!literal_should_be_unquoted("Graph"));
        assert!(!literal_should_be_unquoted("PowerShell"));
    }
}

#[cfg(test)]
mod version_partial_tests {
    use super::*;
    use tfls_core::version_constraint::{ConstraintOp, CursorSlot};

    #[test]
    fn inside_version_yields_typed_digits() {
        let slot = CursorSlot::InsideVersion {
            op: ConstraintOp::Gte,
            partial: "4.7".to_string(),
        };
        assert_eq!(version_partial_of(&slot), "4.7");
    }

    #[test]
    fn after_operator_yields_empty() {
        let slot = CursorSlot::AfterOperator(ConstraintOp::Gte);
        assert_eq!(version_partial_of(&slot), "");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tftest_completion_tests {
    use super::*;
    use tfls_parser::parse_source;

    #[test]
    fn reference_prefix_detects_namespaces() {
        assert_eq!(
            tftest_reference_prefix("  region = var."),
            Some(("var", ""))
        );
        assert_eq!(
            tftest_reference_prefix("  condition = output.i"),
            Some(("output", "i"))
        );
        assert_eq!(tftest_reference_prefix("  x = run."), Some(("run", "")));
    }

    #[test]
    fn reference_prefix_handles_multibyte_delimiter_without_panicking() {
        // A multibyte non-alphanumeric char immediately before the token
        // (em-dash, smart quote, NBSP) must not panic the byte slice.
        assert_eq!(tftest_reference_prefix("  x = —var."), Some(("var", "")));
        assert_eq!(tftest_reference_prefix("»output.i"), Some(("output", "i")));
        assert_eq!(tftest_reference_prefix("\u{00a0}run."), Some(("run", "")));
        // A multibyte char that ISN'T a delimiter boundary still must not panic.
        assert_eq!(tftest_reference_prefix("café"), None);
    }

    #[test]
    fn reference_prefix_ignores_non_reference_and_deep_paths() {
        assert_eq!(tftest_reference_prefix("  command = pl"), None);
        // A second segment (`run.x.`) isn't a value-completion position here.
        assert_eq!(tftest_reference_prefix("  x = run.x."), None);
        // `local.` isn't a test-file namespace.
        assert_eq!(tftest_reference_prefix("  x = local."), None);
    }

    #[test]
    fn resolve_schema_for_run_offers_command() {
        let schema = tftest_resolve_schema(&["run".to_string()]).expect("run schema");
        assert!(schema.attrs.iter().any(|a| a.name == "command"));
        assert!(schema.blocks.iter().any(|b| b.name == "assert"));
    }

    #[test]
    fn resolve_schema_descends_into_assert() {
        let schema =
            tftest_resolve_schema(&["run".to_string(), "assert".to_string()]).expect("assert");
        assert!(schema.attrs.iter().any(|a| a.name == "condition"));
        assert!(schema.attrs.iter().any(|a| a.name == "error_message"));
    }

    #[test]
    fn resolve_schema_rejects_resource() {
        // `.tf` blocks never resolve through the test grammar.
        assert!(tftest_resolve_schema(&["resource".to_string()]).is_none());
    }

    #[test]
    fn block_path_includes_top_level() {
        let src = "run \"x\" {\n  assert {\n    \n  }\n}\n";
        let body = parse_source(src).body.expect("parses");
        // Cursor on the blank line inside `assert`.
        let offset = src.find("    \n").expect("blank line") + 4;
        assert_eq!(tftest_block_path(&body, offset), vec!["run", "assert"]);
    }

    #[test]
    fn top_level_items_offer_run_not_resource() {
        let items = tftest_top_level_items();
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"run"));
        assert!(labels.contains(&"mock_provider"));
        assert!(!labels.contains(&"resource"));
    }

    #[test]
    fn run_labels_collected_from_body() {
        let src = "run \"first\" {}\nrun \"second\" {}\n";
        let body = parse_source(src).body.expect("parses");
        let labels: Vec<String> = tftest_run_label_items(Some(&body))
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert_eq!(labels, vec!["first", "second"]);
    }
}
