//! `terraform-ls/searchDocs` and `terraform-ls/getDoc` — free-text search
//! across loaded provider schemas, and full-docs retrieval by name.
//!
//! These are custom LSP extension methods (server-namespaced under
//! `terraform-ls/`, advertised via `ServerCapabilities.experimental`).
//! They let clients build a "what resource should I use?" search UX
//! without round-tripping through `textDocument/completion`, which is
//! prefix-on-name only.
//!
//! Matching is an AND over whitespace-separated terms; each term must
//! appear (case-insensitive substring) in at least one of: resource
//! name, first-line summary, full description body, or an attribute
//! name. Per-term scores take the maximum weight across hit fields,
//! with name > summary > description > attributes, plus a shortness
//! bonus so `aws_s3_bucket` outranks `aws_s3_bucket_policy` for query
//! `s3 bucket`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tfls_core::ProviderAddress;
use tfls_schema::{BlockSchema, NestedBlockSchema, NestingMode, ProviderSchema, Schema};
use tfls_state::StateStore;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

// --- Wire types ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Resource,
    Data,
    /// Language-level built-in block — `terraform`, `variable`,
    /// `output`, `module`, `backend`, `cloud`, etc. `GetDocParams.name`
    /// is a dot-joined path through the built-in tree:
    /// * `terraform`
    /// * `terraform.backend.s3`
    /// * `terraform.cloud.workspaces`
    /// * `variable`
    /// * `lifecycle.resource`
    ///
    /// Resolved via `tfls_core::resolve_nested_schema`.
    Builtin,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchDocsParams {
    pub query: String,
    #[serde(default)]
    pub kinds: Option<Vec<Kind>>,
    #[serde(default)]
    pub providers: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchDocsResult {
    pub items: Vec<SearchDocsItem>,
    pub total: u32,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchDocsItem {
    pub name: String,
    pub kind: Kind,
    pub provider: String,
    pub summary: String,
    pub score: f32,
    pub matched_fields: Vec<MatchedField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchedField {
    Name,
    Summary,
    Description,
    Attribute,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetDocParams {
    pub name: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, Serialize)]
pub struct GetDocResult {
    pub name: String,
    pub kind: Kind,
    pub provider: String,
    pub markdown: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetSnippetParams {
    pub name: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, Serialize)]
pub struct GetSnippetResult {
    pub name: String,
    pub kind: Kind,
    pub provider: String,
    /// LSP snippet-format string ready for `lsp_expand`. Includes the
    /// leading `resource "` / `data "` prefix so clients can expand it
    /// as a standalone block insertion.
    pub snippet: String,
}

// --- Defaults and limits ---------------------------------------------------

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

// Per-field weights for each query term. Max across hit fields wins.
const W_NAME: f32 = 5.0;
const W_SUMMARY: f32 = 3.0;
const W_DESCRIPTION: f32 = 1.5;
const W_ATTRIBUTE: f32 = 1.0;

// --- Handlers --------------------------------------------------------------

pub async fn search_docs(
    backend: &Backend,
    params: SearchDocsParams,
) -> jsonrpc::Result<SearchDocsResult> {
    let query = params.query.trim().to_ascii_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let want_resource = match &params.kinds {
        None => true,
        Some(ks) => ks.contains(&Kind::Resource),
    };
    let want_data = match &params.kinds {
        None => true,
        Some(ks) => ks.contains(&Kind::Data),
    };
    let provider_filter: Option<Vec<String>> = params
        .providers
        .map(|v| v.into_iter().map(|s| s.to_ascii_lowercase()).collect());

    let mut scored: Vec<(f32, SearchDocsItem)> = Vec::new();

    for entry in backend.state.schemas.iter() {
        let addr = entry.key();
        let schema = entry.value();
        let provider_name = addr.r#type.to_ascii_lowercase();
        if let Some(filter) = &provider_filter {
            if !filter.iter().any(|p| p == &provider_name) {
                continue;
            }
        }

        if want_resource {
            for (name, res) in &schema.resource_schemas {
                if let Some((score, item)) =
                    score_item(name, Kind::Resource, &res.block, addr, schema, &terms)
                {
                    scored.push((score, item));
                }
            }
        }
        if want_data {
            for (name, ds) in &schema.data_source_schemas {
                if let Some((score, item)) =
                    score_item(name, Kind::Data, &ds.block, addr, schema, &terms)
                {
                    scored.push((score, item));
                }
            }
        }
    }

    let total = scored.len() as u32;
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let truncated = (scored.len() as u32) > limit;
    let items: Vec<SearchDocsItem> = scored
        .into_iter()
        .take(limit as usize)
        .map(|(_, i)| i)
        .collect();

    Ok(SearchDocsResult {
        items,
        total,
        truncated,
    })
}

pub async fn get_doc(
    backend: &Backend,
    params: GetDocParams,
) -> jsonrpc::Result<GetDocResult> {
    let name = params.name;
    let kind = params.kind;

    if kind == Kind::Builtin {
        return get_doc_builtin(name);
    }

    let (addr, provider, block) = match kind {
        Kind::Resource => match find_entry(&backend.state, Kind::Resource, &name) {
            Some(x) => x,
            None => return Err(not_found(&name, kind)),
        },
        Kind::Data => match find_entry(&backend.state, Kind::Data, &name) {
            Some(x) => x,
            None => return Err(not_found(&name, kind)),
        },
        Kind::Builtin => unreachable!("handled above"),
    };

    let markdown = render_full_doc(&name, kind, &provider, &block.block);
    let registry_url = registry_link(&addr, kind, &name);

    Ok(GetDocResult {
        name,
        kind,
        provider: addr.r#type.clone(),
        markdown,
        registry_url,
    })
}

/// Resolve a dot-joined built-in path (e.g. `terraform.backend.s3`)
/// against the built-in schema tree and render a full-doc markdown
/// view. Reuses the same `## Required` / `## Optional` / `## Nested
/// Blocks` shape as provider-schema docs, but sources every field
/// from `tfls_core::builtin_blocks` instead of the live provider
/// schema.
fn get_doc_builtin(name: String) -> jsonrpc::Result<GetDocResult> {
    // Split the path. Keywords are dot-separated; labels would go in
    // brackets if we supported them, but none of the current
    // label-driven blocks (`backend "s3"`, `provider_meta "aws"`)
    // need labels here since `resolve_nested_schema`'s last step
    // treats a bare keyword at a labelled slot as "give me the
    // default / example schema for this block family".
    let steps: Vec<tfls_core::BlockStep> = name
        .split('.')
        .filter(|s| !s.is_empty())
        .map(|keyword| tfls_core::BlockStep {
            keyword: keyword.to_string(),
            label: None,
        })
        .collect();
    if steps.is_empty() {
        return Err(jsonrpc::Error::invalid_params(format!(
            "empty built-in path: {name}"
        )));
    }

    // For label-bearing blocks at the leaf (backend "s3" etc.), hand
    // through to the label dispatcher directly so the returned schema
    // reflects the specific backend.
    let schema = if steps.len() >= 2 && steps[steps.len() - 2].keyword == "backend" {
        tfls_core::builtin_blocks::backend_schema(&steps[steps.len() - 1].keyword)
    } else {
        tfls_core::resolve_nested_schema(&steps)
    };

    let Some(schema) = schema else {
        return Err(jsonrpc::Error::invalid_params(format!(
            "unknown built-in path: {name}"
        )));
    };

    let markdown = render_full_builtin_doc(&name, &steps, &schema);
    Ok(GetDocResult {
        name,
        kind: Kind::Builtin,
        provider: String::new(),
        markdown,
        registry_url: None,
    })
}

pub async fn get_snippet(
    backend: &Backend,
    params: GetSnippetParams,
) -> jsonrpc::Result<GetSnippetResult> {
    let name = params.name;
    let kind = params.kind;

    // Snippets for built-in blocks would be a separate feature —
    // `terraform { required_providers { … } }` scaffolding etc. For
    // now reject to keep the wire shape explicit; the provider-
    // schema path is unchanged.
    if kind == Kind::Builtin {
        return Err(jsonrpc::Error::invalid_params(
            "get_snippet does not support Kind::Builtin yet".to_string(),
        ));
    }

    let addr = match find_entry(&backend.state, kind, &name) {
        Some((addr, _, _)) => addr,
        None => return Err(not_found(&name, kind)),
    };

    let kind_keyword = match kind {
        Kind::Resource => "resource",
        Kind::Data => "data",
        Kind::Builtin => unreachable!("handled above"),
    };
    // `resource_scaffold_snippet` returns the tail after `<kind> "` —
    // starting with the type name and the closing quote. Prepend the
    // keyword so the client gets a complete insertable block.
    let tail = crate::handlers::completion::resource_scaffold_snippet(&name, backend, kind_keyword);
    let snippet = format!("{kind_keyword} \"{tail}");

    Ok(GetSnippetResult {
        name,
        kind,
        provider: addr.r#type.clone(),
        snippet,
    })
}

// --- Lookup / scoring ------------------------------------------------------

fn score_item(
    name: &str,
    kind: Kind,
    block: &BlockSchema,
    addr: &ProviderAddress,
    _schema: &Arc<ProviderSchema>,
    terms: &[&str],
) -> Option<(f32, SearchDocsItem)> {
    let name_lc = name.to_ascii_lowercase();
    let desc = block.description.clone().unwrap_or_default();
    let summary = first_line(&desc).to_string();
    let summary_lc = summary.to_ascii_lowercase();
    let desc_lc = desc.to_ascii_lowercase();
    let attrs_joined = block
        .attributes
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();

    // Empty query: every item matches with a baseline score.
    if terms.is_empty() {
        return Some((
            shortness_bonus(&name_lc),
            build_item(name, kind, addr, summary, 0.0, Vec::new()),
        ));
    }

    let mut total = 0.0f32;
    let mut matched = Vec::<MatchedField>::new();

    for term in terms {
        let mut best = 0.0f32;
        let mut best_field: Option<MatchedField> = None;
        if name_lc.contains(term) {
            best = W_NAME;
            best_field = Some(MatchedField::Name);
        }
        if summary_lc.contains(term) && W_SUMMARY > best {
            best = W_SUMMARY;
            best_field = Some(MatchedField::Summary);
        }
        if desc_lc.contains(term) && W_DESCRIPTION > best {
            best = W_DESCRIPTION;
            best_field = Some(MatchedField::Description);
        }
        if attrs_joined.contains(term) && W_ATTRIBUTE > best {
            best = W_ATTRIBUTE;
            best_field = Some(MatchedField::Attribute);
        }
        if best == 0.0 {
            // Every term must match something.
            return None;
        }
        total += best;
        if let Some(f) = best_field {
            if !matched.contains(&f) {
                matched.push(f);
            }
        }
    }

    total += shortness_bonus(&name_lc);
    let max_possible = (W_NAME * terms.len() as f32) + 1.0;
    let normalised = (total / max_possible).min(1.0);

    Some((normalised, build_item(name, kind, addr, summary, normalised, matched)))
}

fn build_item(
    name: &str,
    kind: Kind,
    addr: &ProviderAddress,
    summary: String,
    score: f32,
    matched_fields: Vec<MatchedField>,
) -> SearchDocsItem {
    SearchDocsItem {
        name: name.to_string(),
        kind,
        provider: addr.r#type.clone(),
        summary,
        score,
        matched_fields,
        registry_url: registry_link(addr, kind, name),
    }
}

fn shortness_bonus(name_lc: &str) -> f32 {
    // Shorter names score slightly higher so exact/near-exact matches
    // don't lose to longer siblings (e.g. aws_s3_bucket vs aws_s3_bucket_policy).
    if name_lc.is_empty() {
        0.0
    } else {
        1.0 / (name_lc.len() as f32)
    }
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
}

fn find_entry(
    state: &StateStore,
    kind: Kind,
    name: &str,
) -> Option<(ProviderAddress, String, Schema)> {
    for entry in state.schemas.iter() {
        let addr = entry.key();
        let ps = entry.value();
        let hit = match kind {
            Kind::Resource => ps.resource_schemas.get(name).cloned(),
            Kind::Data => ps.data_source_schemas.get(name).cloned(),
            // Built-ins don't live in provider schemas.
            Kind::Builtin => None,
        };
        if let Some(schema) = hit {
            return Some((addr.clone(), addr.r#type.clone(), schema));
        }
    }
    None
}

fn not_found(name: &str, kind: Kind) -> jsonrpc::Error {
    let label = match kind {
        Kind::Resource => "resource",
        Kind::Data => "data source",
        Kind::Builtin => "built-in block",
    };
    let mut err = jsonrpc::Error::invalid_params(format!(
        "no {} named `{}` in any loaded provider schema",
        label, name
    ));
    err.code = jsonrpc::ErrorCode::ServerError(-32001);
    err
}

// --- Markdown synthesis ----------------------------------------------------

fn render_full_doc(name: &str, kind: Kind, _provider: &str, block: &BlockSchema) -> String {
    let kind_label = match kind {
        Kind::Resource => "Resource",
        Kind::Data => "Data Source",
        // Built-ins are rendered by `render_full_builtin_doc` on a
        // separate path; this function only sees provider schemas.
        Kind::Builtin => "Built-in",
    };
    let mut out = String::new();
    out.push_str(&format!("# `{}`\n\n", name));
    out.push_str(&format!("_{}_\n\n", kind_label));

    if let Some(desc) = &block.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }

    let (required, optional, computed) = partition_attributes(block);

    if !required.is_empty() {
        out.push_str("## Required\n\n");
        for (aname, attr) in required {
            out.push_str(&format!("- `{}` ", aname));
            if let Some(desc) = &attr.description {
                let d = desc.trim();
                if !d.is_empty() {
                    out.push_str("— ");
                    out.push_str(d);
                }
            }
            out.push('\n');
        }
        out.push('\n');
    }
    if !optional.is_empty() {
        out.push_str("## Optional\n\n");
        for (aname, attr) in optional {
            out.push_str(&format!("- `{}` ", aname));
            if let Some(desc) = &attr.description {
                let d = desc.trim();
                if !d.is_empty() {
                    out.push_str("— ");
                    out.push_str(d);
                }
            }
            out.push('\n');
        }
        out.push('\n');
    }
    if !computed.is_empty() {
        out.push_str("## Read-Only\n\n");
        for (aname, attr) in computed {
            out.push_str(&format!("- `{}` ", aname));
            if let Some(desc) = &attr.description {
                let d = desc.trim();
                if !d.is_empty() {
                    out.push_str("— ");
                    out.push_str(d);
                }
            }
            out.push('\n');
        }
        out.push('\n');
    }

    if !block.block_types.is_empty() {
        out.push_str("## Nested Blocks\n\n");
        let mut entries: Vec<(&String, &NestedBlockSchema)> =
            block.block_types.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (nested_name, nb) in entries {
            render_nested_block_summary(&mut out, nested_name, nb);
        }
    }

    out
}

/// Full-doc markdown renderer for built-in blocks (terraform /
/// variable / output / backend "s3" / cloud.workspaces / …).
/// Mirrors the shape of `render_full_doc` — heading, kind label,
/// description, Required/Optional sections, Nested Blocks — but
/// pulls every field from `BuiltinSchema` / `BuiltinAttr` /
/// `BuiltinBlock` instead of the provider schema.
fn render_full_builtin_doc(
    name: &str,
    steps: &[tfls_core::BlockStep],
    schema: &tfls_core::builtin_blocks::BuiltinSchema,
) -> String {
    use tfls_core::builtin_blocks::BuiltinBlock;

    let mut out = String::new();
    out.push_str(&format!("# `{name}`\n\n"));
    out.push_str("_Built-in block_\n\n");

    // Lookup the parent's BuiltinBlock entry for this leaf (so we can
    // show the one-line `detail` summary as the description). Only
    // when the path goes at least one level beyond a root; top-level
    // roots (`terraform`, `variable`, …) don't have a pre-written
    // detail line, so we synthesise a brief description from the
    // block keyword instead.
    let parent_block: Option<BuiltinBlock> = if steps.len() >= 2 {
        // Walk to the parent and look up the block entry by name.
        let parent_steps = &steps[..steps.len() - 1];
        let parent_schema = tfls_core::resolve_nested_schema(parent_steps);
        let leaf_name = &steps[steps.len() - 1].keyword;
        parent_schema.and_then(|ps| {
            // Special-case backend: the step-wise resolver doesn't
            // name `backend.s3` as a block on backend, because
            // `backend` is label-dispatched. Synthesize a virtual
            // entry.
            if parent_steps.last().map(|s| s.keyword.as_str()) == Some("backend") {
                None
            } else {
                ps.blocks.iter().find(|b| b.name == leaf_name).copied()
            }
        })
    } else {
        None
    };

    let parent_detail =
        parent_block.and_then(|b| (!b.detail.is_empty()).then_some(b.detail));
    if let Some(detail) = parent_detail {
        out.push_str(detail);
        out.push_str("\n\n");
    } else if steps.len() == 1 {
        let desc = match steps[0].keyword.as_str() {
            "terraform" => "Top-level configuration block. \
                Holds the required_version pin, required_providers map, \
                optional backend / cloud / provider_meta sub-blocks, and \
                language experiments.",
            "variable" => "Declare a module input variable. \
                Supports `type`, `default`, `description`, `sensitive`, \
                `nullable`, and a `validation { }` sub-block.",
            "output" => "Declare a module output. \
                `value` is required; `description`, `sensitive`, \
                `depends_on`, and `precondition {}` are optional.",
            "module" => "Invoke a child module. \
                `source` is required; `version` applies to registry modules \
                only; `providers` maps child-module provider keys to the \
                parent's.",
            "lifecycle" => "Customise how Terraform manages resource lifecycle \
                (create_before_destroy, prevent_destroy, ignore_changes, \
                replace_triggered_by), plus precondition / postcondition \
                assertions.",
            _ => "",
        };
        if !desc.is_empty() {
            out.push_str(desc);
            out.push_str("\n\n");
        }
    }

    // Required attrs.
    let required: Vec<&tfls_core::builtin_blocks::BuiltinAttr> =
        schema.attrs.iter().filter(|a| a.required).collect();
    let optional: Vec<&tfls_core::builtin_blocks::BuiltinAttr> =
        schema.attrs.iter().filter(|a| !a.required).collect();

    if !required.is_empty() {
        out.push_str("## Required\n\n");
        for a in &required {
            out.push_str(&format!("- `{}` ", a.name));
            if !a.detail.is_empty() {
                out.push_str("— ");
                out.push_str(a.detail);
            }
            out.push('\n');
        }
        out.push('\n');
    }
    if !optional.is_empty() {
        out.push_str("## Optional\n\n");
        for a in &optional {
            out.push_str(&format!("- `{}` ", a.name));
            if !a.detail.is_empty() {
                out.push_str("— ");
                out.push_str(a.detail);
            }
            out.push('\n');
        }
        out.push('\n');
    }

    if !schema.blocks.is_empty() {
        out.push_str("## Nested Blocks\n\n");
        // Sort for a deterministic view across runs.
        let mut entries: Vec<&BuiltinBlock> = schema.blocks.iter().collect();
        entries.sort_by_key(|b| b.name);
        for b in entries {
            render_builtin_nested_block_summary(&mut out, b);
        }
    }

    out
}

/// One nested-block sub-section for `render_full_builtin_doc` —
/// mirrors `render_nested_block_summary` for provider schemas but
/// reads from the built-in structs.
fn render_builtin_nested_block_summary(
    out: &mut String,
    block: &tfls_core::builtin_blocks::BuiltinBlock,
) {
    let schema = block.body_schema();
    let has_attrs = schema.is_some_and(|s| !s.attrs.is_empty());
    let has_sub = schema.is_some_and(|s| !s.blocks.is_empty());
    let has_detail = !block.detail.is_empty();

    if !has_detail && !has_attrs && !has_sub {
        out.push_str(&format!("- `{}`\n\n", block.name));
        return;
    }

    out.push_str(&format!("### `{}`", block.name));
    // Cardinality-equivalent: label-bearing vs not. Built-ins don't
    // carry nesting-mode/min/max — they're all singletons or
    // label-dispatched.
    if block.label_placeholder.is_some() {
        out.push_str(&format!(
            " — labelled (e.g. `{name} \"{label}\" {{ … }}`)",
            name = block.name,
            label = block.label_placeholder.unwrap_or(""),
        ));
    }
    out.push_str("\n\n");

    if has_detail {
        out.push_str(block.detail);
        out.push_str("\n\n");
    }

    if let Some(sch) = schema {
        let required: Vec<&tfls_core::builtin_blocks::BuiltinAttr> =
            sch.attrs.iter().filter(|a| a.required).collect();
        let optional: Vec<&tfls_core::builtin_blocks::BuiltinAttr> =
            sch.attrs.iter().filter(|a| !a.required).collect();
        if !required.is_empty() {
            out.push_str("- **Required:**\n");
            for a in &required {
                out.push_str(&format!("  - `{}`", a.name));
                if !a.detail.is_empty() {
                    out.push_str(" — ");
                    out.push_str(a.detail);
                }
                out.push('\n');
            }
        }
        if !optional.is_empty() {
            out.push_str("- **Optional:**\n");
            for a in &optional {
                out.push_str(&format!("  - `{}`", a.name));
                if !a.detail.is_empty() {
                    out.push_str(" — ");
                    out.push_str(a.detail);
                }
                out.push('\n');
            }
        }
        if !sch.blocks.is_empty() {
            let mut names: Vec<&'static str> =
                sch.blocks.iter().map(|b| b.name).collect();
            names.sort();
            let joined: Vec<String> =
                names.iter().map(|n| format!("`{n}`")).collect();
            out.push_str(&format!("- **Sub-blocks:** {}\n", joined.join(", ")));
        }
    }

    out.push('\n');
}

/// Render one nested-block entry as a sub-section under
/// `## Nested Blocks`. Shows the block's nesting mode + cardinality,
/// its description (when provided by the schema), and the names of
/// its required / optional / read-only attributes plus any sub-
/// blocks. Stops at one level — deeper structure is reachable via
/// the per-block-header hover in `hover_attribute.rs`.
///
/// When the schema for a nested block is fully empty (no
/// description, no attrs, no sub-blocks — rare, but possible for
/// provider-side vestigial entries), fall back to a single
/// bare-name bullet rather than emitting an empty sub-heading.
fn render_nested_block_summary(out: &mut String, name: &str, nb: &NestedBlockSchema) {
    let has_desc = nb
        .block
        .description
        .as_deref()
        .is_some_and(|d| !d.trim().is_empty());
    let has_attrs = !nb.block.attributes.is_empty();
    let has_sub = !nb.block.block_types.is_empty();

    if !has_desc && !has_attrs && !has_sub {
        out.push_str(&format!("- `{}`\n\n", name));
        return;
    }

    out.push_str(&format!("### `{}`", name));

    // Inline flags: nesting mode, min/max items, deprecation.
    // Rendered as em-dash-prefixed plain text rather than
    // `_italic_` because neovim's markdown hover view doesn't
    // reliably italicise an underscore run that sits immediately
    // after an inline-code span (see screenshot 2026-04-21); plain
    // text renders cleanly everywhere.
    let mut flags: Vec<String> = Vec::new();
    flags.push(nesting_mode_label(nb.nesting_mode).to_string());
    if nb.min_items > 0 {
        flags.push(format!("min {}", nb.min_items));
    }
    if nb.max_items > 0 {
        flags.push(format!("max {}", nb.max_items));
    }
    if nb.block.deprecated {
        flags.push("deprecated".to_string());
    }
    out.push_str(&format!(" — {}\n\n", flags.join(", ")));

    if let Some(desc) = nb.block.description.as_deref().filter(|_| has_desc) {
        out.push_str(desc.trim());
        out.push_str("\n\n");
    }

    if has_attrs {
        let (required, optional, computed) = partition_attributes(&nb.block);
        if !required.is_empty() {
            write_nested_attr_section(out, "Required", &required);
        }
        if !optional.is_empty() {
            write_nested_attr_section(out, "Optional", &optional);
        }
        if !computed.is_empty() {
            write_nested_attr_section(out, "Read-Only", &computed);
        }
    }

    if has_sub {
        let mut sub_names: Vec<&String> = nb.block.block_types.keys().collect();
        sub_names.sort();
        let joined: Vec<String> = sub_names.iter().map(|n| format!("`{n}`")).collect();
        out.push_str(&format!("- **Sub-blocks:** {}\n", joined.join(", ")));
    }

    out.push('\n');
}

fn nesting_mode_label(mode: NestingMode) -> &'static str {
    match mode {
        NestingMode::Single => "single",
        NestingMode::List => "list",
        NestingMode::Set => "set",
        NestingMode::Map => "map",
        NestingMode::Group => "group",
    }
}

/// Emit a per-attribute sub-list under a Required / Optional /
/// Read-Only heading for a nested block. Matches the attribute
/// descriptions the parent doc already shows under `## Required`
/// etc — users shouldn't have to dig deeper than this hover to
/// know what each nested-block attr does.
///
/// Indented with two spaces so markdown renderers treat the attr
/// bullets as children of the section label bullet.
fn write_nested_attr_section(
    out: &mut String,
    label: &str,
    attrs: &[(&str, &tfls_schema::AttributeSchema)],
) {
    out.push_str(&format!("- **{}:**\n", label));
    for (name, attr) in attrs {
        out.push_str(&format!("  - `{}`", name));
        if let Some(desc) = &attr.description {
            let d = desc.trim();
            if !d.is_empty() {
                out.push_str(" — ");
                out.push_str(d);
            }
        }
        out.push('\n');
    }
}

type AttrList<'a> = Vec<(&'a str, &'a tfls_schema::AttributeSchema)>;

fn partition_attributes(
    block: &BlockSchema,
) -> (AttrList<'_>, AttrList<'_>, AttrList<'_>) {
    let mut required: Vec<(&str, &tfls_schema::AttributeSchema)> = Vec::new();
    let mut optional: Vec<(&str, &tfls_schema::AttributeSchema)> = Vec::new();
    let mut computed: Vec<(&str, &tfls_schema::AttributeSchema)> = Vec::new();
    for (name, attr) in &block.attributes {
        if attr.required {
            required.push((name.as_str(), attr));
        } else if attr.optional {
            optional.push((name.as_str(), attr));
        } else if attr.computed {
            computed.push((name.as_str(), attr));
        } else {
            optional.push((name.as_str(), attr));
        }
    }
    required.sort_by_key(|(n, _)| *n);
    optional.sort_by_key(|(n, _)| *n);
    computed.sort_by_key(|(n, _)| *n);
    (required, optional, computed)
}

// --- Registry URL ----------------------------------------------------------

fn registry_link(addr: &ProviderAddress, kind: Kind, type_name: &str) -> Option<String> {
    if addr.hostname != "registry.terraform.io" {
        return None;
    }
    let segment = match kind {
        Kind::Resource => "resources",
        Kind::Data => "data-sources",
        // Built-ins don't have per-block registry pages.
        Kind::Builtin => return None,
    };
    Some(format!(
        "https://registry.terraform.io/providers/{}/{}/latest/docs/{}/{}",
        addr.namespace, addr.r#type, segment, type_name
    ))
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_schema::{AttributeSchema, BlockSchema, ProviderSchema, Schema};

    fn make_schema(description: &str, attrs: &[(&str, bool)]) -> Schema {
        let mut block = BlockSchema {
            description: Some(description.to_string()),
            ..Default::default()
        };
        for (name, required) in attrs {
            let attr = AttributeSchema {
                required: *required,
                optional: !*required,
                ..Default::default()
            };
            block.attributes.insert(name.to_string(), attr);
        }
        Schema { version: 1, block }
    }

    fn state_with_azurerm() -> StateStore {
        let state = StateStore::new();
        let mut ps = ProviderSchema {
            provider: Schema {
                version: 1,
                block: BlockSchema::default(),
            },
            resource_schemas: Default::default(),
            data_source_schemas: Default::default(),
        };
        ps.resource_schemas.insert(
            "azurerm_kubernetes_cluster".to_string(),
            make_schema(
                "Manages a Managed Kubernetes Cluster (AKS).\n\nMore docs...",
                &[("name", true), ("location", true), ("tags", false)],
            ),
        );
        ps.resource_schemas.insert(
            "azurerm_storage_account".to_string(),
            make_schema(
                "Manages an Azure Storage Account.",
                &[("name", true), ("account_tier", true)],
            ),
        );
        ps.data_source_schemas.insert(
            "azurerm_storage_account".to_string(),
            make_schema(
                "Gets information about an existing Storage Account.",
                &[("name", true)],
            ),
        );
        let addr = ProviderAddress::hashicorp("azurerm");
        state.schemas.insert(addr, Arc::new(ps));
        state
    }

    #[tokio::test]
    async fn search_by_name_prefix_hits_expected_resource() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let result = search_docs(
            &backend,
            SearchDocsParams {
                query: "kubernetes".to_string(),
                kinds: None,
                providers: None,
                limit: None,
            },
        )
        .await
        .unwrap();
        assert!(result.items.iter().any(|i| i.name == "azurerm_kubernetes_cluster"));
    }

    #[tokio::test]
    async fn search_by_description_finds_non_name_match() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let result = search_docs(
            &backend,
            SearchDocsParams {
                query: "managed cluster".to_string(),
                kinds: None,
                providers: None,
                limit: None,
            },
        )
        .await
        .unwrap();
        let first = result.items.first().expect("at least one match");
        assert_eq!(first.name, "azurerm_kubernetes_cluster");
        assert!(first.matched_fields.contains(&MatchedField::Description)
            || first.matched_fields.contains(&MatchedField::Summary));
    }

    #[tokio::test]
    async fn search_kind_filter_data_only_excludes_resources() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let result = search_docs(
            &backend,
            SearchDocsParams {
                query: "storage".to_string(),
                kinds: Some(vec![Kind::Data]),
                providers: None,
                limit: None,
            },
        )
        .await
        .unwrap();
        assert!(result.items.iter().all(|i| i.kind == Kind::Data));
    }

    #[tokio::test]
    async fn search_provider_filter_unknown_provider_returns_empty() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let result = search_docs(
            &backend,
            SearchDocsParams {
                query: "storage".to_string(),
                kinds: None,
                providers: Some(vec!["aws".to_string()]),
                limit: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.total, 0);
        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn search_term_and_logic_requires_all_terms() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let result = search_docs(
            &backend,
            SearchDocsParams {
                query: "kubernetes storage".to_string(),
                kinds: None,
                providers: None,
                limit: None,
            },
        )
        .await
        .unwrap();
        // Neither item contains BOTH terms — result must be empty.
        assert_eq!(result.total, 0);
    }

    #[tokio::test]
    async fn get_doc_returns_markdown_with_required_section() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let result = get_doc(
            &backend,
            GetDocParams {
                name: "azurerm_kubernetes_cluster".to_string(),
                kind: Kind::Resource,
            },
        )
        .await
        .unwrap();
        assert!(result.markdown.contains("azurerm_kubernetes_cluster"));
        assert!(result.markdown.contains("## Required"));
        assert!(result.markdown.contains("name"));
        assert!(result.markdown.contains("location"));
    }

    #[tokio::test]
    async fn get_doc_nested_blocks_include_description_cardinality_and_attrs() {
        // Regression: the "Nested Blocks" section previously emitted
        // just a bare list of block names. Users had to close the
        // doc, insert the nested block, then hover on the block
        // header to learn what the block does or which attrs it
        // takes. Now the full doc includes per-block description,
        // nesting mode, cardinality, and attr summaries inline.
        let state = StateStore::new();
        let mut ps = ProviderSchema {
            provider: Schema {
                version: 1,
                block: BlockSchema::default(),
            },
            resource_schemas: Default::default(),
            data_source_schemas: Default::default(),
        };
        let mut nested_block = BlockSchema {
            description: Some("Customize details about the root block device.".to_string()),
            ..Default::default()
        };
        nested_block.attributes.insert(
            "volume_size".to_string(),
            AttributeSchema {
                optional: true,
                description: Some("Size of the volume in GiB.".to_string()),
                ..Default::default()
            },
        );
        nested_block.attributes.insert(
            "volume_type".to_string(),
            AttributeSchema {
                required: true,
                description: Some("gp2, gp3, io1 or similar.".to_string()),
                ..Default::default()
            },
        );
        let mut resource_block = BlockSchema {
            description: Some("An EC2 instance.".to_string()),
            ..Default::default()
        };
        resource_block.attributes.insert(
            "ami".to_string(),
            AttributeSchema { required: true, ..Default::default() },
        );
        resource_block.block_types.insert(
            "root_block_device".to_string(),
            NestedBlockSchema {
                nesting_mode: NestingMode::List,
                block: nested_block,
                min_items: 0,
                max_items: 1,
            },
        );
        ps.resource_schemas.insert(
            "aws_instance".to_string(),
            Schema { version: 1, block: resource_block },
        );
        let addr = ProviderAddress::hashicorp("aws");
        state.schemas.insert(addr, Arc::new(ps));

        let backend = make_backend(state);
        let result = get_doc(
            &backend,
            GetDocParams {
                name: "aws_instance".to_string(),
                kind: Kind::Resource,
            },
        )
        .await
        .unwrap();
        let md = &result.markdown;

        assert!(md.contains("## Nested Blocks"), "md: {md}");
        // Sub-heading per nested block.
        assert!(md.contains("### `root_block_device`"), "md: {md}");
        // Nesting mode + cardinality flag.
        assert!(
            md.contains("### `root_block_device` — list, max 1"),
            "expected em-dash cardinality flag; md: {md}"
        );
        // Description paragraph.
        assert!(
            md.contains("Customize details about the root block device."),
            "expected description; md: {md}"
        );
        // Required + Optional sub-lists with per-attr descriptions.
        assert!(md.contains("- **Required:**\n"), "md: {md}");
        assert!(
            md.contains("  - `volume_type` — gp2, gp3, io1 or similar."),
            "expected required attr with description; md: {md}"
        );
        assert!(md.contains("- **Optional:**\n"), "md: {md}");
        assert!(
            md.contains("  - `volume_size` — Size of the volume in GiB."),
            "expected optional attr with description; md: {md}"
        );
    }

    #[tokio::test]
    async fn get_doc_empty_nested_block_falls_back_to_bare_bullet() {
        // When a nested block has no description, no attrs, and no
        // sub-blocks, an empty sub-section would look worse than a
        // bare-name bullet. Guarded by the empty-fallback branch.
        let state = StateStore::new();
        let mut ps = ProviderSchema {
            provider: Schema {
                version: 1,
                block: BlockSchema::default(),
            },
            resource_schemas: Default::default(),
            data_source_schemas: Default::default(),
        };
        let mut outer = BlockSchema::default();
        outer.attributes.insert(
            "name".to_string(),
            AttributeSchema { required: true, ..Default::default() },
        );
        outer.block_types.insert(
            "vestigial".to_string(),
            NestedBlockSchema {
                nesting_mode: NestingMode::Single,
                block: BlockSchema::default(),
                min_items: 0,
                max_items: 0,
            },
        );
        ps.resource_schemas.insert(
            "some_resource".to_string(),
            Schema { version: 1, block: outer },
        );
        state
            .schemas
            .insert(ProviderAddress::hashicorp("some"), Arc::new(ps));

        let backend = make_backend(state);
        let md = get_doc(
            &backend,
            GetDocParams {
                name: "some_resource".to_string(),
                kind: Kind::Resource,
            },
        )
        .await
        .unwrap()
        .markdown;

        assert!(md.contains("## Nested Blocks"), "md: {md}");
        assert!(
            md.contains("- `vestigial`"),
            "empty nested block should fall back to bare bullet; md: {md}"
        );
        assert!(
            !md.contains("### `vestigial`"),
            "should not emit empty sub-heading; md: {md}"
        );
    }

    #[tokio::test]
    async fn get_doc_for_terraform_root_returns_builtin_markdown() {
        // Built-in root: `terraform` renders the top-level terraform
        // block doc — required_version + experiments attrs, plus
        // required_providers / backend / cloud / provider_meta as
        // nested blocks.
        let state = StateStore::new();
        let backend = make_backend(state);
        let result = get_doc(
            &backend,
            GetDocParams {
                name: "terraform".to_string(),
                kind: Kind::Builtin,
            },
        )
        .await
        .unwrap();
        assert!(result.markdown.contains("# `terraform`"), "{}", result.markdown);
        assert!(
            result.markdown.contains("_Built-in block_"),
            "{}",
            result.markdown
        );
        assert!(
            result.markdown.contains("## Nested Blocks"),
            "{}",
            result.markdown
        );
        // Expect all four nested block names to be listed.
        for name in ["required_providers", "backend", "cloud", "provider_meta"] {
            assert!(
                result.markdown.contains(name),
                "expected nested block {name} in: {}",
                result.markdown
            );
        }
    }

    #[tokio::test]
    async fn get_doc_for_backend_s3_returns_full_builtin_markdown() {
        let state = StateStore::new();
        let backend = make_backend(state);
        let result = get_doc(
            &backend,
            GetDocParams {
                name: "terraform.backend.s3".to_string(),
                kind: Kind::Builtin,
            },
        )
        .await
        .unwrap();
        assert!(
            result.markdown.contains("# `terraform.backend.s3`"),
            "{}",
            result.markdown
        );
        assert!(result.markdown.contains("## Required"), "{}", result.markdown);
        assert!(result.markdown.contains("`bucket`"), "{}", result.markdown);
        assert!(result.markdown.contains("`key`"), "{}", result.markdown);
        assert!(result.markdown.contains("## Optional"), "{}", result.markdown);
        // s3 has `region`, `profile`, `encrypt`, etc. as optionals.
        assert!(
            result.markdown.contains("`region`")
                || result.markdown.contains("`encrypt`"),
            "{}",
            result.markdown
        );
        // registry_url should be None for built-ins (no registry page).
        assert!(result.registry_url.is_none());
    }

    #[tokio::test]
    async fn get_doc_for_variable_block_returns_builtin_markdown() {
        let state = StateStore::new();
        let backend = make_backend(state);
        let result = get_doc(
            &backend,
            GetDocParams {
                name: "variable".to_string(),
                kind: Kind::Builtin,
            },
        )
        .await
        .unwrap();
        assert!(result.markdown.contains("# `variable`"), "{}", result.markdown);
        // variable has a `validation` nested block.
        assert!(
            result.markdown.contains("## Nested Blocks"),
            "{}",
            result.markdown
        );
        assert!(
            result.markdown.contains("validation"),
            "{}",
            result.markdown
        );
    }

    #[tokio::test]
    async fn get_doc_for_unknown_builtin_path_returns_error() {
        let state = StateStore::new();
        let backend = make_backend(state);
        let err = get_doc(
            &backend,
            GetDocParams {
                name: "not.a.real.path".to_string(),
                kind: Kind::Builtin,
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{err:?}").contains("not.a.real.path"));
    }

    #[tokio::test]
    async fn get_doc_unknown_returns_error() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let err = get_doc(
            &backend,
            GetDocParams {
                name: "nonexistent_resource".to_string(),
                kind: Kind::Resource,
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{:?}", err).contains("nonexistent_resource"));
    }

    #[tokio::test]
    async fn get_snippet_with_required_attrs_has_tabstops_for_each() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let result = get_snippet(
            &backend,
            GetSnippetParams {
                name: "azurerm_kubernetes_cluster".to_string(),
                kind: Kind::Resource,
            },
        )
        .await
        .unwrap();
        assert!(result.snippet.starts_with("resource \"azurerm_kubernetes_cluster\""));
        assert!(result.snippet.contains("${1:name}"));
        // Required attrs `name` and `location` are alphabetised, so they
        // take tabstops ${2} and ${3} respectively.
        assert!(result.snippet.contains("location = \"${2}\""));
        assert!(result.snippet.contains("name = \"${3}\""));
        // With required attrs present the snippet must NOT carry `$0` —
        // the natural exit is after the last tabstop. Matches the
        // completion handler's behavior.
        assert!(!result.snippet.contains("$0"));
        assert!(result.snippet.ends_with('}'));
    }

    #[tokio::test]
    async fn get_snippet_data_source_uses_data_prefix() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let result = get_snippet(
            &backend,
            GetSnippetParams {
                name: "azurerm_storage_account".to_string(),
                kind: Kind::Data,
            },
        )
        .await
        .unwrap();
        assert!(result.snippet.starts_with("data \"azurerm_storage_account\""));
        assert!(result.snippet.contains("${1:name}"));
        assert_eq!(result.kind, Kind::Data);
    }

    #[tokio::test]
    async fn get_snippet_no_required_attrs_exits_with_dollar_zero() {
        let state = StateStore::new();
        let mut ps = ProviderSchema {
            provider: Schema { version: 1, block: BlockSchema::default() },
            resource_schemas: Default::default(),
            data_source_schemas: Default::default(),
        };
        // Resource with only optional attrs — no required fields.
        let mut block = BlockSchema::default();
        let opt = AttributeSchema { optional: true, ..Default::default() };
        block.attributes.insert("length".to_string(), opt);
        ps.resource_schemas.insert(
            "random_pet".to_string(),
            Schema { version: 1, block },
        );
        let addr = ProviderAddress::hashicorp("random");
        state.schemas.insert(addr, Arc::new(ps));
        let backend = make_backend(state);
        let result = get_snippet(
            &backend,
            GetSnippetParams {
                name: "random_pet".to_string(),
                kind: Kind::Resource,
            },
        )
        .await
        .unwrap();
        assert!(result.snippet.contains("$0"));
    }

    #[tokio::test]
    async fn get_snippet_unknown_returns_error() {
        let state = state_with_azurerm();
        let backend = make_backend(state);
        let err = get_snippet(
            &backend,
            GetSnippetParams {
                name: "nope_not_a_thing".to_string(),
                kind: Kind::Resource,
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{:?}", err).contains("nope_not_a_thing"));
    }

    fn make_backend(state: StateStore) -> Backend {
        use tfls_state::JobQueue;
        let (service, _) = tower_lsp::LspService::new(Backend::new);
        let client = service.inner().client.clone();
        Backend::with_shared_state(client, Arc::new(state), Arc::new(JobQueue::new()))
    }
}
