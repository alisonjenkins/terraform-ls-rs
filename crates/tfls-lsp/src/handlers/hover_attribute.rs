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
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Url};
use std::sync::Arc;
use tfls_core::builtin_blocks::{BuiltinAttr, LIFECYCLE_DATA_BLOCK, LIFECYCLE_RESOURCE_BLOCK};
use tfls_parser::hcl_span_to_lsp_range;
use tfls_schema::{AttributeSchema, BlockSchema, NestedBlockSchema, NestingMode, ProviderSchema};
use tfls_state::{DocumentState, StateStore};

/// Try to produce a hover for an attribute key under the cursor. Always
/// returns `Some(...)` when the cursor is on an attribute key inside a
/// resource / data / provider body — even if the relevant schema isn't
/// loaded, so the user gets a clear "what to do" message instead of
/// silently falling through to the enclosing block's label hover.
pub fn attribute_hover(
    state: &StateStore,
    doc: &DocumentState,
    pos: Position,
    uri: &Url,
) -> Option<Hover> {
    let body = doc.parsed.body.as_ref()?;
    let hit = find_hit_at(body, doc, pos)?;

    match hit {
        Hit::Attribute(hit) => {
            // Meta-block attributes (`lifecycle { create_before_destroy = … }`,
            // `lifecycle.precondition.condition`, `provisioner`, `connection`)
            // aren't in provider schemas — they're Terraform/OpenTofu language
            // constructs. Route those to a dedicated renderer so we don't
            // falsely claim "attribute is not in the schema for aws_foo".
            // Also handle top-level meta-args (count / for_each /
            // depends_on / provider) that sit directly on a resource/data
            // body — they aren't in the provider schema either.
            // Built-in roots (terraform / variable / output / module):
            // attempt a built-in attribute hover. If the attr isn't
            // in the built-in schema AND the root is `module`, return
            // None so the module-input hover path downstream can
            // resolve it to the child module's variable declaration.
            // For terraform / variable / output the fallback is an
            // "unknown attribute" placeholder — those roots don't
            // have a downstream hover handler that could do better.
            if let Some(keyword) = hit.root_kind.builtin_root_keyword() {
                if let Some(md) = render_builtin_attribute_for_hit(&hit, keyword) {
                    let range = hcl_span_to_lsp_range(&doc.rope, hit.key_span).ok()?;
                    return Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: md,
                        }),
                        range: Some(range),
                    });
                }
                return None;
            }
            let markdown = if matches!(hit.root_kind, RootBlockKind::Provider)
                && hit.nested_path.is_empty()
                && provider_meta_attr(&hit.attr_name).is_some()
            {
                // Provider meta-attrs (`alias`, `version`) aren't in
                // the provider's own config schema — they're Terraform-
                // language additions declared in
                // `PROVIDER_BLOCK_META_ATTRS`.
                render_provider_meta_attr(&hit)
            } else if is_meta_block_path(&hit.nested_path) {
                render_meta_attribute(&hit, uri)
            } else if hit.nested_path.is_empty()
                && tfls_core::is_meta_attr(&hit.attr_name)
            {
                render_top_level_meta_arg(&hit)
            } else {
                match resolve_attribute_schema(state, &hit) {
                    AttributeLookup::Found(schema) => render_attribute(&hit, &schema),
                    AttributeLookup::SchemasNotLoaded => render_schemas_not_loaded(&hit),
                    AttributeLookup::ProviderMissing => render_provider_missing(&hit),
                    AttributeLookup::AttributeUnknown => render_attribute_unknown(&hit),
                }
            };
            let range = hcl_span_to_lsp_range(&doc.rope, hit.key_span).ok()?;
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: markdown,
                }),
                range: Some(range),
            })
        }
        Hit::NestedBlockHeader(hit) => {
            let markdown = if hit.root_kind.builtin_root_keyword().is_some() {
                render_builtin_nested_block(&hit)
            } else {
                match resolve_nested_block_schema(state, &hit) {
                    NestedBlockLookup::Found(nb) => render_nested_block(&hit, &nb),
                    NestedBlockLookup::SchemasNotLoaded => {
                        render_nested_block_schemas_not_loaded(&hit)
                    }
                    NestedBlockLookup::ProviderMissing => render_nested_block_provider_missing(&hit),
                    NestedBlockLookup::BlockUnknown => render_nested_block_unknown(&hit),
                }
            };
            let range = hcl_span_to_lsp_range(&doc.rope, hit.ident_span).ok()?;
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: markdown,
                }),
                range: Some(range),
            })
        }
    }
}

/// Cursor landed on either an attribute key or a nested-block header —
/// these render differently.
enum Hit {
    Attribute(AttributeHit),
    NestedBlockHeader(NestedBlockHeaderHit),
}

struct NestedBlockHeaderHit {
    root_kind: RootBlockKind,
    root_type: String,
    /// Path from the root block down to (but not including) the
    /// block whose header the cursor sits on. Empty when the cursor
    /// is on an immediate child of the root body.
    parent_path: Vec<String>,
    /// Name of the nested block whose header we're hovering on.
    block_name: String,
    ident_span: std::ops::Range<usize>,
}

/// True if the nested path passes through a meta-block whose contents
/// are language-level, not provider-schema-defined.
fn is_meta_block_path(path: &[String]) -> bool {
    path.first().is_some_and(|n| {
        matches!(
            n.as_str(),
            "lifecycle" | "provisioner" | "connection"
        )
    })
}

fn render_meta_attribute(hit: &AttributeHit, uri: &Url) -> String {
    // Lifecycle is the block we actually have built-in schemas for;
    // provisioner / connection contents are too varied to model and
    // get a generic "language meta-argument" note.
    let path_join = hit.nested_path.join(".");
    if hit.nested_path.first().map(|s| s.as_str()) == Some("lifecycle") {
        return render_lifecycle_attribute(hit, uri, &path_join);
    }
    format!(
        "**attribute** `{attr}` in `{path}` (Terraform/OpenTofu language meta-argument)\n\n\
         Meta-block contents aren't provider-defined; their valid keys are part of the Terraform / OpenTofu language specification rather than the provider schema.",
        attr = hit.attr_name,
        path = path_join,
    )
}

fn render_lifecycle_attribute(hit: &AttributeHit, uri: &Url, path: &str) -> String {
    // Pick the right lifecycle schema per outer block kind.
    let schema_attrs: &[BuiltinAttr] = match hit.root_kind {
        RootBlockKind::Resource => LIFECYCLE_RESOURCE_BLOCK.attrs,
        RootBlockKind::DataSource => LIFECYCLE_DATA_BLOCK.attrs,
        // `lifecycle` doesn't belong in any non-resource/data block;
        // fall back to the resource list for tolerance so we never
        // panic if we somehow end up here.
        RootBlockKind::Provider
        | RootBlockKind::Terraform
        | RootBlockKind::Variable
        | RootBlockKind::Output
        | RootBlockKind::Module => LIFECYCLE_RESOURCE_BLOCK.attrs,
    };
    // Prefer the richer language-reference description from tfls_core
    // (multi-line markdown); fall back to the shorter `BuiltinAttr.detail`
    // if we don't have one for this name.
    let kind_for_lookup = match hit.root_kind {
        RootBlockKind::DataSource => tfls_core::BlockKind::Data,
        _ => tfls_core::BlockKind::Resource,
    };
    let detail: Option<String> = if hit.nested_path.len() == 1 {
        let rich = tfls_core::lifecycle_attr_description(kind_for_lookup, &hit.attr_name);
        if !rich.is_empty() {
            Some(rich.to_string())
        } else {
            schema_attrs
                .iter()
                .find(|a| a.name == hit.attr_name)
                .map(|a| a.detail.to_string())
        }
    } else if matches!(
        hit.nested_path.get(1).map(|s| s.as_str()),
        Some("precondition") | Some("postcondition")
    ) {
        let rich = tfls_core::condition_attr_description(&hit.attr_name);
        if !rich.is_empty() {
            Some(rich.to_string())
        } else {
            None
        }
    } else {
        None
    };

    let mut out = format!(
        "**attribute** `{attr}` in `{path}`",
        attr = hit.attr_name,
        path = path,
    );
    if let Some(d) = &detail {
        out.push_str(&format!("\n\n{d}"));
    }

    // `enabled` is OpenTofu-only. Surface that fact plainly in the
    // hover so users don't have to cross-reference the inlay hint.
    if hit.attr_name == "enabled" {
        let tofu_file = is_opentofu_file(uri);
        if tofu_file {
            out.push_str("\n\n_OpenTofu 1.11+ meta-argument. ([docs](https://opentofu.org/docs/language/meta-arguments/enabled/))_");
        } else {
            out.push_str(
                "\n\n⚠ _OpenTofu 1.11+ meta-argument — Terraform does not support it. \
Rename this file to `.tofu` (or `.tofu.json`) if this module is OpenTofu-only, \
or use `count = var.create ? 1 : 0` / `for_each` for Terraform compatibility. \
([docs](https://opentofu.org/docs/language/meta-arguments/enabled/))_",
            );
        }
    }

    out
}

fn is_opentofu_file(uri: &Url) -> bool {
    let path = uri.path();
    path.ends_with(".tofu") || path.ends_with(".tofu.json")
}

/// Render hover markdown for an attribute inside a built-in root
/// block (`terraform`, `variable`, `output`, `module`, and their
/// deeper nested blocks). Returns `Some(markdown)` when the
/// attribute is known in the built-in schema tree. Returns `None`
/// for unknown attrs on `module` roots so the caller can delegate
/// to the module-input hover path (which resolves `region = ...`
/// to the referenced child module's variable declaration). Other
/// roots surface an "unknown attribute" placeholder rather than
/// `None`, since they have no downstream hover handler that could
/// do better.
fn render_builtin_attribute_for_hit(hit: &AttributeHit, root_keyword: &str) -> Option<String> {
    let schema = resolve_builtin_schema_for_hit(root_keyword, &hit.nested_path);
    let attr = schema.and_then(|s| s.attrs.iter().find(|a| a.name == hit.attr_name).copied());

    if attr.is_none() && matches!(hit.root_kind, RootBlockKind::Module) {
        return None;
    }

    let path_suffix = if hit.nested_path.is_empty() {
        root_keyword.to_string()
    } else {
        format!("{root_keyword}.{}", hit.nested_path.join("."))
    };

    let mut out = format!(
        "**attribute** `{attr}` in `{path}`",
        attr = hit.attr_name,
        path = path_suffix,
    );

    match attr {
        Some(a) => {
            if a.required {
                out.push_str(" — required");
            }
            if !a.detail.is_empty() {
                out.push_str("\n\n");
                out.push_str(a.detail);
            }
        }
        None => {
            out.push_str("\n\n_Unknown attribute for this built-in block._");
        }
    }
    Some(out)
}

/// Render hover for the identifier of a nested built-in block
/// inside any built-in root (`terraform`, `variable`, `output`,
/// `module`).
fn render_builtin_nested_block(hit: &NestedBlockHeaderHit) -> String {
    let Some(root_keyword) = hit.root_kind.builtin_root_keyword() else {
        return render_nested_block_unknown(hit);
    };
    // Walk from the root → through parent_path → to the target
    // block's own slot, so the returned schema is the block body
    // the cursor opens into.
    let mut steps: Vec<tfls_core::BlockStep> = Vec::with_capacity(hit.parent_path.len() + 2);
    steps.push(tfls_core::BlockStep {
        keyword: root_keyword.to_string(),
        label: None,
    });
    for keyword in &hit.parent_path {
        steps.push(tfls_core::BlockStep {
            keyword: keyword.clone(),
            label: None,
        });
    }
    steps.push(tfls_core::BlockStep {
        keyword: hit.block_name.clone(),
        label: None,
    });
    let schema = tfls_core::resolve_nested_schema(&steps);

    // Also look up the *parent*'s BuiltinBlock entry to read the
    // block's own `detail` line (which lives on the parent's
    // `.blocks[name]`, not on the resolved body schema).
    let parent_schema = resolve_builtin_schema_for_hit(root_keyword, &hit.parent_path);
    let block_info = parent_schema.and_then(|s| {
        s.blocks
            .iter()
            .find(|b| b.name == hit.block_name)
            .copied()
    });

    let root_label = match hit.root_kind {
        RootBlockKind::Terraform => "terraform block".to_string(),
        RootBlockKind::Variable => format!("variable `{}`", hit.root_type),
        RootBlockKind::Output => format!("output `{}`", hit.root_type),
        RootBlockKind::Module => format!("module `{}`", hit.root_type),
        _ => "built-in block".to_string(),
    };
    let mut out = format!("**block** `{name}`", name = hit.block_name);
    if hit.parent_path.is_empty() {
        out.push_str(&format!(" on {root_label}"));
    } else {
        out.push_str(&format!(
            " inside `{root_keyword}.{}`",
            hit.parent_path.join("."),
        ));
    }

    // Block detail from the parent's BuiltinBlock entry (one-line
    // summary). Skipped silently when the parent doesn't have a
    // matching entry (unusual but tolerated).
    if let Some(info) = block_info {
        if !info.detail.is_empty() {
            out.push_str("\n\n");
            out.push_str(info.detail);
        }
        if info.label_placeholder.is_some() {
            out.push_str(&format!(
                "\n\n_Labelled block — example: `{name} \"{label}\" {{ … }}`_",
                name = hit.block_name,
                label = info.label_placeholder.unwrap_or(""),
            ));
        }
    }

    // Attributes + sub-blocks from the resolved body schema.
    if let Some(sch) = schema {
        if !sch.attrs.is_empty() {
            let required: Vec<&'static str> = sch
                .attrs
                .iter()
                .filter(|a| a.required)
                .map(|a| a.name)
                .collect();
            let optional: Vec<&'static str> = sch
                .attrs
                .iter()
                .filter(|a| !a.required)
                .map(|a| a.name)
                .collect();
            if !required.is_empty() {
                out.push_str("\n\n**Required attrs:** ");
                out.push_str(
                    &required
                        .iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            if !optional.is_empty() {
                out.push_str("\n\n**Optional attrs:** ");
                out.push_str(
                    &optional
                        .iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
        }
        if !sch.blocks.is_empty() {
            let names: Vec<&'static str> = sch.blocks.iter().map(|b| b.name).collect();
            out.push_str("\n\n**Nested blocks:** ");
            out.push_str(
                &names
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    } else if block_info.is_none() {
        out.push_str("\n\n_Unknown nested block for this terraform tree._");
    }

    out
}

/// Walk the built-in schema tree for a given root keyword
/// (`terraform` / `variable` / `output` / `module`) down `path`
/// (block keywords only — labels aren't currently picked up for
/// built-in hover resolution). Returns the schema at the end of the
/// path, or `None` if any step doesn't match.
fn resolve_builtin_schema_for_hit(
    root_keyword: &str,
    path: &[String],
) -> Option<tfls_core::builtin_blocks::BuiltinSchema> {
    let mut steps: Vec<tfls_core::BlockStep> = Vec::with_capacity(path.len() + 1);
    steps.push(tfls_core::BlockStep {
        keyword: root_keyword.to_string(),
        label: None,
    });
    for keyword in path {
        steps.push(tfls_core::BlockStep {
            keyword: keyword.clone(),
            label: None,
        });
    }
    tfls_core::resolve_nested_schema(&steps)
}

/// Look up an attribute in `PROVIDER_BLOCK_META_ATTRS` by name.
/// Returns `None` for names not in the meta list.
fn provider_meta_attr(
    name: &str,
) -> Option<&'static tfls_core::builtin_blocks::BuiltinAttr> {
    tfls_core::builtin_blocks::PROVIDER_BLOCK_META_ATTRS
        .iter()
        .find(|a| a.name == name)
}

/// Render hover for a provider meta-attribute (`alias`, `version`
/// — both declared in `PROVIDER_BLOCK_META_ATTRS`, both never in
/// the provider's own config schema).
fn render_provider_meta_attr(hit: &AttributeHit) -> String {
    let attr = match provider_meta_attr(&hit.attr_name) {
        Some(a) => a,
        None => return render_attribute_unknown(hit),
    };
    let mut out = format!(
        "**meta-argument** `{attr}` on provider `{root}`",
        attr = hit.attr_name,
        root = hit.root_type,
    );
    if attr.required {
        out.push_str(" — required");
    }
    if !attr.detail.is_empty() {
        out.push_str("\n\n");
        out.push_str(attr.detail);
    }
    out
}

/// Render hover for a top-level meta-argument
/// (`count`/`for_each`/`provider`/`depends_on`) sitting directly on
/// a `resource`/`data` body — none of these appear in the provider
/// schema, so the normal attribute-hover path would falsely report
/// "not in the schema". Pulled from
/// `tfls_core::meta_attr_description` which is also used to
/// populate the completion documentation popup, so the two surfaces
/// stay consistent.
fn render_top_level_meta_arg(hit: &AttributeHit) -> String {
    let kind_label = match hit.root_kind {
        RootBlockKind::Resource => "resource",
        RootBlockKind::DataSource => "data source",
        RootBlockKind::Provider => "provider",
        // Meta-args aren't used directly on built-in blocks; these
        // branches are dead in practice but keep the match
        // exhaustive.
        RootBlockKind::Terraform => "terraform",
        RootBlockKind::Variable => "variable",
        RootBlockKind::Output => "output",
        RootBlockKind::Module => "module",
    };
    let body = tfls_core::meta_attr_description(&hit.attr_name);
    format!(
        "**meta-argument** `{attr}` on {kind} `{root}`\n\n{body}",
        attr = hit.attr_name,
        kind = kind_label,
        root = hit.root_type,
    )
}

enum AttributeLookup {
    Found(AttributeSchema),
    /// No schemas at all — user probably hasn't run `terraform init`.
    SchemasNotLoaded,
    /// Schemas are loaded but none of them provide this resource type.
    ProviderMissing,
    /// Schema exists for the enclosing block but the attribute isn't in it.
    AttributeUnknown,
}

fn resolve_attribute_schema(state: &StateStore, hit: &AttributeHit) -> AttributeLookup {
    if state.schemas.is_empty() {
        return AttributeLookup::SchemasNotLoaded;
    }

    let root_schema = match hit.root_kind {
        RootBlockKind::Resource => state
            .find_resource_schema(&hit.root_type)
            .and_then(|ps| ps.resource_schemas.get(&hit.root_type).cloned()),
        RootBlockKind::DataSource => state
            .find_data_source_schema(&hit.root_type)
            .and_then(|ps| ps.data_source_schemas.get(&hit.root_type).cloned()),
        RootBlockKind::Provider => {
            find_provider_schema(state, &hit.root_type).map(|ps| ps.provider.clone())
        }
        // Built-in-root hits are routed through `render_builtin_*`
        // before reaching this function.
        RootBlockKind::Terraform
        | RootBlockKind::Variable
        | RootBlockKind::Output
        | RootBlockKind::Module => return AttributeLookup::AttributeUnknown,
    };
    let Some(root_schema) = root_schema else {
        return AttributeLookup::ProviderMissing;
    };

    let Some(block_schema) = descend_schema(&root_schema.block, &hit.nested_path) else {
        return AttributeLookup::AttributeUnknown;
    };
    match block_schema.attributes.get(&hit.attr_name).cloned() {
        Some(a) => AttributeLookup::Found(a),
        None => AttributeLookup::AttributeUnknown,
    }
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
/// look the attribute up as a resource, data source, or provider attribute,
/// or whether it's a built-in language block (`terraform`, `variable`,
/// `output`, `module`).
#[derive(Debug, Clone, Copy)]
enum RootBlockKind {
    Resource,
    DataSource,
    Provider,
    Terraform,
    Variable,
    Output,
    Module,
}

impl RootBlockKind {
    /// For built-in roots: the root keyword that
    /// `tfls_core::resolve_nested_schema` uses to pick the starting
    /// schema. Returns `None` for provider-schema-driven roots.
    fn builtin_root_keyword(self) -> Option<&'static str> {
        match self {
            RootBlockKind::Terraform => Some("terraform"),
            RootBlockKind::Variable => Some("variable"),
            RootBlockKind::Output => Some("output"),
            RootBlockKind::Module => Some("module"),
            _ => None,
        }
    }
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

fn find_hit_at(body: &Body, doc: &DocumentState, pos: Position) -> Option<Hit> {
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
            "terraform" => RootBlockKind::Terraform,
            "variable" => RootBlockKind::Variable,
            "output" => RootBlockKind::Output,
            "module" => RootBlockKind::Module,
            _ => continue,
        };

        // `terraform` is unlabelled — synthesise a stable
        // `AttributeHit.root_type` so the rest of the scanner can
        // stay label-aware without special casing. variable / output
        // / module DO carry a label (the declared name) — fall
        // through to the normal label lookup.
        let type_label = match root_kind {
            RootBlockKind::Terraform => "terraform".to_string(),
            _ => match block.labels.first().map(label_text) {
                Some(label) => label,
                None => continue,
            },
        };

        if !span_contains(block.span(), offset) {
            continue;
        }

        if let Some(hit) = scan_block(&block.body, offset, &mut Vec::new(), root_kind, &type_label)
        {
            return Some(hit);
        }
    }
    None
}

fn scan_block(
    body: &Body,
    offset: usize,
    path: &mut Vec<String>,
    root_kind: RootBlockKind,
    root_type: &str,
) -> Option<Hit> {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if attribute_key_contains(attr, offset) {
                let key_span = attr.key.span()?;
                return Some(Hit::Attribute(AttributeHit {
                    root_kind,
                    root_type: root_type.to_string(),
                    nested_path: path.clone(),
                    attr_name: attr.key.as_str().to_string(),
                    key_span,
                }));
            }
            continue;
        }
        let Some(block) = structure.as_block() else {
            continue;
        };
        if !span_contains(block.span(), offset) {
            continue;
        }
        let name = block.ident.as_str().to_string();
        // Cursor on the nested block's header keyword — hover that.
        if let Some(ident_span) = block.ident.span() {
            if span_contains(Some(ident_span.clone()), offset) {
                return Some(Hit::NestedBlockHeader(NestedBlockHeaderHit {
                    root_kind,
                    root_type: root_type.to_string(),
                    parent_path: path.clone(),
                    block_name: name,
                    ident_span,
                }));
            }
        }

        // `dynamic "<label>" { content { … } }` — for schema-lookup
        // purposes treat this as a plain `<label> { … }` block.
        // Push the label (not "dynamic") onto the path, and step
        // through the `content {}` wrapper without pushing anything
        // for it so the recursive scan lands on the attribute's
        // target nested-block schema.
        if name == "dynamic" {
            let Some(label) = block.labels.first().map(hover_label_text) else {
                // Malformed — stop descending.
                return None;
            };
            path.push(label);
            // Walk the dynamic body; if the cursor is inside the
            // content { } wrapper, recurse into its body so attrs
            // resolve to the target block's schema.
            let hit = scan_block_dynamic_body(
                &block.body,
                offset,
                path,
                root_kind,
                root_type,
            );
            path.pop();
            return hit;
        }

        path.push(name);
        let hit = scan_block(&block.body, offset, path, root_kind, root_type);
        path.pop();
        if hit.is_some() {
            return hit;
        }
    }
    None
}

/// Walk the body of a `dynamic "<label>" {}` block. `path` is
/// already set to the pushed target label; on `content {}` we
/// recurse into its body without further push. On attrs directly
/// on the dynamic body (`for_each`, `iterator`, `labels`) we
/// return a hit that downstream hover can route specifically.
fn scan_block_dynamic_body(
    body: &Body,
    offset: usize,
    path: &mut Vec<String>,
    root_kind: RootBlockKind,
    root_type: &str,
) -> Option<Hit> {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if attribute_key_contains(attr, offset) {
                let key_span = attr.key.span()?;
                // Attrs sit on the dynamic construct itself —
                // report a synthetic AttributeHit with the pushed
                // nested_path so downstream hover sees this the
                // same as a plain-block meta-attr.
                return Some(Hit::Attribute(AttributeHit {
                    root_kind,
                    root_type: root_type.to_string(),
                    nested_path: path.clone(),
                    attr_name: attr.key.as_str().to_string(),
                    key_span,
                }));
            }
            continue;
        }
        let Some(child) = structure.as_block() else {
            continue;
        };
        if !span_contains(child.span(), offset) {
            continue;
        }
        if child.ident.as_str() == "content" {
            // Cursor on `content` keyword itself — report it as a
            // nested-block header so downstream hover can special-
            // case it with dynamic-body docs.
            if let Some(ident_span) = child.ident.span() {
                if span_contains(Some(ident_span.clone()), offset) {
                    return Some(Hit::NestedBlockHeader(NestedBlockHeaderHit {
                        root_kind,
                        root_type: root_type.to_string(),
                        parent_path: path.clone(),
                        block_name: "content".to_string(),
                        ident_span,
                    }));
                }
            }
            // Cursor inside the content body — recurse without
            // pushing "content" onto the path.
            return scan_block(&child.body, offset, path, root_kind, root_type);
        }
        // Any other block inside dynamic body is malformed — don't
        // descend, let the caller's outer scan handle it.
    }
    None
}

fn hover_label_text(label: &hcl_edit::structure::BlockLabel) -> String {
    match label {
        hcl_edit::structure::BlockLabel::String(s) => s.value().to_string(),
        hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
    }
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
        // Built-in-root hits are handled by render_builtin_attribute
        // before this function runs; fall back to "built-in" for
        // defensive exhaustiveness.
        RootBlockKind::Terraform
        | RootBlockKind::Variable
        | RootBlockKind::Output
        | RootBlockKind::Module => "built-in",
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
        // Em-dash + plain text — see render_nested_block for the
        // renderer quirk that breaks `_..._` after inline-code.
        out.push_str(&format!(" — {}", flags.join(", ")));
    }
    out.push('\n');

    if let Some(desc) = schema.description.as_deref() {
        if !desc.trim().is_empty() {
            out.push('\n');
            out.push_str(desc);
        }
    }

    // Relational metadata from the provider schema (present on the CLI JSON
    // path for providers that declare them). Plugin-protocol schemas don't
    // currently carry these — the fields stay empty and this block is skipped.
    append_related(&mut out, "Conflicts with", &schema.conflicts_with);
    append_related(&mut out, "Required with", &schema.required_with);
    append_related(&mut out, "Exactly one of", &schema.exactly_one_of);
    append_related(&mut out, "At least one of", &schema.at_least_one_of);

    out
}

fn append_related(out: &mut String, label: &str, names: &[String]) {
    if names.is_empty() {
        return;
    }
    let list: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    out.push_str(&format!("\n\n**{label}:** {}", list.join(", ")));
}

fn attribute_header(hit: &AttributeHit) -> String {
    let kind = match hit.root_kind {
        RootBlockKind::Resource => "resource",
        RootBlockKind::DataSource => "data source",
        RootBlockKind::Provider => "provider",
        RootBlockKind::Terraform
        | RootBlockKind::Variable
        | RootBlockKind::Output
        | RootBlockKind::Module => "built-in",
    };
    let mut out = format!("**attribute** `{attr}` on {kind} `{root}`",
        attr = hit.attr_name,
        root = hit.root_type);
    if !hit.nested_path.is_empty() {
        out.push_str(&format!(" (block `{}`)", hit.nested_path.join(".")));
    }
    out
}

fn render_schemas_not_loaded(hit: &AttributeHit) -> String {
    format!(
        "{header}\n\n_No provider schemas are loaded._ \
Run `terraform init` (or `tofu init`) in this workspace so tfls can fetch \
attribute documentation via `terraform providers schema -json`.",
        header = attribute_header(hit),
    )
}

fn render_provider_missing(hit: &AttributeHit) -> String {
    format!(
        "{header}\n\n_No schema for `{root}` is loaded._ \
The relevant provider may not be declared in `required_providers`, or \
`terraform init` has not been run since it was added.",
        header = attribute_header(hit),
        root = hit.root_type,
    )
}

fn render_attribute_unknown(hit: &AttributeHit) -> String {
    format!(
        "{header}\n\n_Attribute `{attr}` is not in the schema for `{root}`._ \
Check the spelling or provider version.",
        header = attribute_header(hit),
        attr = hit.attr_name,
        root = hit.root_type,
    )
}

enum NestedBlockLookup {
    Found(NestedBlockSchema),
    SchemasNotLoaded,
    ProviderMissing,
    BlockUnknown,
}

fn resolve_nested_block_schema(state: &StateStore, hit: &NestedBlockHeaderHit) -> NestedBlockLookup {
    if state.schemas.is_empty() {
        return NestedBlockLookup::SchemasNotLoaded;
    }

    let root_schema = match hit.root_kind {
        RootBlockKind::Resource => state
            .find_resource_schema(&hit.root_type)
            .and_then(|ps| ps.resource_schemas.get(&hit.root_type).cloned()),
        RootBlockKind::DataSource => state
            .find_data_source_schema(&hit.root_type)
            .and_then(|ps| ps.data_source_schemas.get(&hit.root_type).cloned()),
        RootBlockKind::Provider => {
            find_provider_schema(state, &hit.root_type).map(|ps| ps.provider.clone())
        }
        // Built-in-root hits are handled by render_builtin_nested_block
        // before this function runs.
        RootBlockKind::Terraform
        | RootBlockKind::Variable
        | RootBlockKind::Output
        | RootBlockKind::Module => return NestedBlockLookup::BlockUnknown,
    };
    let Some(root_schema) = root_schema else {
        return NestedBlockLookup::ProviderMissing;
    };

    let Some(parent) = descend_schema(&root_schema.block, &hit.parent_path) else {
        return NestedBlockLookup::BlockUnknown;
    };
    match parent.block_types.get(&hit.block_name).cloned() {
        Some(nb) => NestedBlockLookup::Found(nb),
        None => NestedBlockLookup::BlockUnknown,
    }
}

fn nested_block_header(hit: &NestedBlockHeaderHit) -> String {
    let kind = match hit.root_kind {
        RootBlockKind::Resource => "resource",
        RootBlockKind::DataSource => "data source",
        RootBlockKind::Provider => "provider",
        // Built-in-root hits go through render_builtin_nested_block;
        // this branch is defensive.
        RootBlockKind::Terraform
        | RootBlockKind::Variable
        | RootBlockKind::Output
        | RootBlockKind::Module => "built-in",
    };
    let mut out = format!(
        "**block** `{block}` on {kind} `{root}`",
        block = hit.block_name,
        kind = kind,
        root = hit.root_type,
    );
    if !hit.parent_path.is_empty() {
        out.push_str(&format!(" (inside `{}`)", hit.parent_path.join(".")));
    }
    out
}

fn render_nested_block(hit: &NestedBlockHeaderHit, nb: &NestedBlockSchema) -> String {
    let mut out = nested_block_header(hit);

    // Nesting + cardinality metadata.
    let nesting = match nb.nesting_mode {
        NestingMode::Single => "single",
        NestingMode::List => "list",
        NestingMode::Set => "set",
        NestingMode::Map => "map",
        NestingMode::Group => "group",
    };
    let mut flags = vec![format!("nesting: {nesting}")];
    if nb.min_items > 0 {
        flags.push(format!("min_items: {}", nb.min_items));
    }
    if nb.max_items > 0 {
        flags.push(format!("max_items: {}", nb.max_items));
    }
    if nb.block.deprecated {
        flags.push("deprecated".to_string());
    }
    // Em-dash separator with plain text. Neovim's markdown hover
    // view doesn't reliably italicise `_..._` after an inline-code
    // span; em-dash + plain text renders cleanly everywhere.
    out.push_str(&format!(" — {}\n", flags.join(", ")));

    if let Some(desc) = nb.block.description.as_deref() {
        if !desc.trim().is_empty() {
            out.push('\n');
            out.push_str(desc);
        }
    }

    // Summarise what attributes the block contains — matches the
    // level of detail users get from the provider registry's block
    // reference pages.
    if !nb.block.attributes.is_empty() {
        let mut required: Vec<&String> = nb
            .block
            .attributes
            .iter()
            .filter(|(_, a)| a.required)
            .map(|(n, _)| n)
            .collect();
        let mut optional: Vec<&String> = nb
            .block
            .attributes
            .iter()
            .filter(|(_, a)| a.optional && !a.required)
            .map(|(n, _)| n)
            .collect();
        required.sort();
        optional.sort();
        if !required.is_empty() {
            out.push_str("\n\n**Required attrs:** ");
            out.push_str(
                &required
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        if !optional.is_empty() {
            out.push_str("\n\n**Optional attrs:** ");
            out.push_str(
                &optional
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }
    if !nb.block.block_types.is_empty() {
        let mut nested: Vec<&String> = nb.block.block_types.keys().collect();
        nested.sort();
        out.push_str("\n\n**Nested blocks:** ");
        out.push_str(
            &nested
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    out
}

fn render_nested_block_schemas_not_loaded(hit: &NestedBlockHeaderHit) -> String {
    format!(
        "{header}\n\n_No provider schemas are loaded._ \
Run `terraform init` (or `tofu init`) so tfls can fetch block documentation.",
        header = nested_block_header(hit),
    )
}

fn render_nested_block_provider_missing(hit: &NestedBlockHeaderHit) -> String {
    format!(
        "{header}\n\n_No schema for `{root}` is loaded._",
        header = nested_block_header(hit),
        root = hit.root_type,
    )
}

fn render_nested_block_unknown(hit: &NestedBlockHeaderHit) -> String {
    format!(
        "{header}\n\n_Block `{block}` is not in the schema for `{root}`._",
        header = nested_block_header(hit),
        block = hit.block_name,
        root = hit.root_type,
    )
}
