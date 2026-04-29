//! Generic block-type rename code action — drives auto-fix for
//! the AWS and Kubernetes "type was renamed, replacement has
//! identical schema" deprecation families.
//!
//! Each rename family registers a `BlockRenameSpec` listing
//! the matching `(block_kind, from, to)` tuple plus the
//! provider-version gate that controls whether the migration
//! is currently applicable. The action's mechanics are
//! identical across families:
//!
//! 1. Block-label rewrite — swap `"<from>"` for `"<to>"` on
//!    every matching `<block_kind> "<from>" "X"` block.
//! 2. Reference rewrite — swap `<from>.X` for `<to>.X` on
//!    every traversal whose head ident matches.
//! 3. (resources only) Per-name `moved { from = <from>.X to =
//!    <to>.X }` block in the module's `moved.tf` for state
//!    migration. `moved` is a no-op for `data` blocks (state
//!    is computed, not managed).
//!
//! Multi-scope emit: Selection / File / Module / Workspace,
//! with the standard `source.fixAll.terraform-ls-rs.<id>`
//! `CodeActionKind` family. Per-module gate caches via the
//! shared `module_constraint_for_provider` helper.
//!
//! Does NOT cover the existing `null_resource → terraform_data`
//! action, which has additional attribute-key renames (e.g.
//! `triggers → triggers_replace`) that aren't part of the
//! generic rename pattern. That action stays in its own code
//! path; future consolidation possible if more attribute-rename
//! cases appear.

use std::path::PathBuf;

use rustc_hash::{FxHashMap, FxHashSet};

use hcl_edit::expr::{Expression, TraversalOperator};
use hcl_edit::repr::Span;
use hcl_edit::structure::{Body, BlockLabel};
use lsp_types::{
    CodeAction, CodeActionOrCommand, CreateFile, CreateFileOptions, DocumentChangeOperation,
    DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, Position, Range, ResourceOp,
    TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
};
use ropey::Rope;
use tfls_parser::{byte_offset_to_lsp_position, hcl_span_to_lsp_range};
use tfls_state::StateStore;

use crate::handlers::code_action::walk_expressions;
use crate::handlers::code_action_scope::{
    Scope, for_each_doc_in_scope, range_intersects, scope_kind,
};

/// One type-rename rule used by the generic auto-fix.
#[derive(Debug, Clone, Copy)]
pub struct BlockRenameSpec {
    pub block_kind: &'static str,
    pub from: &'static str,
    pub to: &'static str,
    pub gate_provider: &'static str,
    pub gate_threshold: &'static str,
    /// How safe `moved { from = <from>.X to = <to>.X }` is to
    /// emit. Wrong answer here is dangerous: a `moved` block
    /// pointing at a destination Terraform can't reach
    /// produces "no matching resource" errors at plan time, or
    /// worse, silently destroys + recreates the resource if
    /// the user dismisses the error.
    pub state_migration: StateMigration,
}

/// What we know about cross-type state migration safety for
/// this rename. Drives whether `build_workspace_edit` emits a
/// `moved` block alongside the type-name rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateMigration {
    /// `<from>` and `<to>` are registered as ALIASES in the
    /// provider source — both names resolve to the same
    /// underlying resource implementation, so state already
    /// points at the same object. Safe to emit `moved` on any
    /// Terraform version. Example: `aws_alb` and `aws_lb` both
    /// register `ResourceLb()` in the AWS provider.
    Aliased,
    /// `<from>` and `<to>` are *different* resources sharing a
    /// migration path through the provider's
    /// `MoveResourceState` implementation. Requires Terraform
    /// 1.8+ (cross-type `moved` support landed in CLI 1.8) AND
    /// provider support. Module's `required_version` must
    /// admit 1.8+ before emit; otherwise skip.
    RequiresTerraform18,
    /// Migration safety unknown or known to need user
    /// verification (schema differs between `from` and `to`,
    /// `MoveResourceState` not implemented, etc.). DO NOT
    /// emit a `moved` block automatically; the action rewrites
    /// labels + references but leaves state migration to the
    /// user (`terraform state mv` or hand-authored `moved`
    /// after `terraform plan` review).
    Manual,
}

impl BlockRenameSpec {
    /// `true` for `resource` specs (state-bearing — `moved`
    /// only meaningful for resources, not data sources).
    fn is_resource(&self) -> bool {
        self.block_kind == "resource"
    }
}

/// All rename rules across providers. Caller-facing tables
/// (AWS / Kubernetes) live in `tfls_diag` as
/// `DeprecationRule` slices for the *diagnostic* path; the
/// auto-fix path needs the slimmer struct above so we mirror
/// them here. Keep in sync with `tfls_diag::AWS_TYPE_RENAMES`
/// and `tfls_diag::KUBERNETES_TYPE_RENAMES`.
const ALL_BLOCK_RENAMES: &[BlockRenameSpec] = &[
    // AWS `aws_alb*` family — see `tfls_diag::AWS_TYPE_RENAMES`.
    // Both names register the same `ResourceLb()` in the AWS
    // provider source — true aliases, state addresses are
    // interchangeable. Safe to emit `moved` unconditionally.
    aws_aliased("aws_alb", "aws_lb"),
    aws_aliased("aws_alb_listener", "aws_lb_listener"),
    aws_aliased("aws_alb_listener_rule", "aws_lb_listener_rule"),
    aws_aliased("aws_alb_target_group", "aws_lb_target_group"),
    aws_aliased("aws_alb_target_group_attachment", "aws_lb_target_group_attachment"),
    // `aws_s3_bucket_object` → `aws_s3_object` is NOT an alias
    // — they're distinct resources with diverging defaults
    // (`force_destroy`, lifecycle alignment with the v4 S3
    // split). Cross-type `moved` requires Terraform 1.8+ AND
    // the AWS provider's `MoveResourceState` (4.x+).
    BlockRenameSpec {
        block_kind: "resource",
        from: "aws_s3_bucket_object",
        to: "aws_s3_object",
        gate_provider: "aws",
        gate_threshold: "4.0.0",
        state_migration: StateMigration::RequiresTerraform18,
    },
    // Kubernetes `_v1` rename family — see
    // `tfls_diag::KUBERNETES_TYPE_RENAMES`. The non-versioned
    // resources predate the explicit-API-version naming
    // convention; the `_v1` variants have schema *differences*
    // (HPA metric APIs, RBAC field shape, ingress backend
    // wrapping, etc.). `MoveResourceState` support is per-
    // resource and per-provider-version. Emit Manual until
    // we have schema-driven safety verification.
    k8s_v1("kubernetes_pod"),
    k8s_v1("kubernetes_deployment"),
    k8s_v1("kubernetes_service"),
    k8s_v1("kubernetes_namespace"),
    k8s_v1("kubernetes_config_map"),
    k8s_v1("kubernetes_secret"),
    k8s_v1("kubernetes_role"),
    k8s_v1("kubernetes_role_binding"),
    k8s_v1("kubernetes_cluster_role"),
    k8s_v1("kubernetes_cluster_role_binding"),
    k8s_v1("kubernetes_persistent_volume"),
    k8s_v1("kubernetes_persistent_volume_claim"),
    k8s_v1("kubernetes_service_account"),
    k8s_v1("kubernetes_stateful_set"),
    k8s_v1("kubernetes_daemonset"),
    k8s_v1("kubernetes_job"),
    k8s_v1("kubernetes_cron_job"),
    k8s_v1("kubernetes_network_policy"),
    k8s_v1("kubernetes_ingress"),
    k8s_v1("kubernetes_horizontal_pod_autoscaler"),
];

/// `aws_X` ↔ `aws_lb_*` true-alias spec helper.
const fn aws_aliased(from: &'static str, to: &'static str) -> BlockRenameSpec {
    BlockRenameSpec {
        block_kind: "resource",
        from,
        to,
        gate_provider: "aws",
        gate_threshold: "1.7.0",
        state_migration: StateMigration::Aliased,
    }
}

/// `kubernetes_X` → `kubernetes_X_v1` shorthand. Can't
/// `format!` in a const context — destination is recovered
/// via `resolved_to` at runtime (appends `_v1`). All k8s
/// renames default to `Manual` state migration: schemas
/// diverge between unversioned and `_v1` variants and
/// `MoveResourceState` support varies per provider version.
const fn k8s_v1(from: &'static str) -> BlockRenameSpec {
    BlockRenameSpec {
        block_kind: "resource",
        from,
        to: "",
        gate_provider: "kubernetes",
        gate_threshold: "2.0.0",
        state_migration: StateMigration::Manual,
    }
}

/// Resolves the spec's `to` name. AWS specs spell it out;
/// kubernetes specs leave it empty and we synthesise
/// `<from>_v1`. Allocated `String` rather than borrowed since
/// the kubernetes path needs to build a fresh string.
fn resolved_to(spec: &BlockRenameSpec) -> String {
    if spec.to.is_empty() && spec.from.starts_with("kubernetes_") {
        format!("{}_v1", spec.from)
    } else {
        spec.to.to_string()
    }
}

/// Diagnostic-attached Instance variant. Used when the LSP
/// client sends a code-action request with the deprecation
/// diagnostic in `params.context.diagnostics` — e.g. the user
/// clicked the WARNING squiggle's lightbulb. Reuses the
/// cursor-variant block lookup (the diag's range start IS
/// where the user is focused) and attaches the originating
/// diagnostic to the returned action so the client can pair
/// them in the lightbulb menu.
pub fn make_replace_block_for_diag(
    state: &StateStore,
    uri: &Url,
    diag: &lsp_types::Diagnostic,
    body: &Body,
    rope: &Rope,
) -> Option<CodeAction> {
    let mut action = make_replace_block_at_cursor(state, uri, diag.range.start, body, rope)?;
    action.diagnostics = Some(vec![diag.clone()]);
    Some(action)
}

/// Cursor-driven Instance variant. Surfaces a single-block
/// `Convert <from>.<name> to <to>` quickfix when the cursor
/// sits inside a deprecated `<from>` block whose spec is
/// gate-supported. Filters reference rewrites to the converted
/// block's name only — the user is migrating ONE resource;
/// other instances of the same `<from>` type stay untouched.
///
/// Returns `None` when no matching block is at the cursor or
/// the spec's gate isn't admitted by the active module.
pub fn make_replace_block_at_cursor(
    state: &StateStore,
    uri: &Url,
    cursor: Position,
    body: &Body,
    rope: &Rope,
) -> Option<CodeAction> {
    use hcl_edit::repr::Span as _;

    let module_dir = crate::handlers::util::parent_dir(uri)?;

    // Find the block at cursor whose `(block_kind, label)`
    // matches a spec.
    let by_kind_label: FxHashMap<(&'static str, &'static str), (usize, &BlockRenameSpec)> =
        ALL_BLOCK_RENAMES
            .iter()
            .enumerate()
            .map(|(i, s)| ((s.block_kind, s.from), (i, s)))
            .collect();

    let mut matched: Option<(usize, &BlockRenameSpec, String, Range)> = None;
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        let kind = block.ident.as_str();
        let Some(label) = block.labels.first() else {
            continue;
        };
        let Some(label_text) = label_str(label) else {
            continue;
        };
        let Some(&(idx, spec)) = by_kind_label.get(&(kind, label_text)) else {
            continue;
        };
        let Some(block_span) = block.span() else { continue };
        let Ok(block_range) = hcl_span_to_lsp_range(rope, block_span) else {
            continue;
        };
        if !crate::handlers::code_action::contains(&block_range, cursor) {
            continue;
        }
        let Some(name) = block.labels.get(1).and_then(label_str) else {
            continue;
        };
        let Some(label_span) = label.span() else { continue };
        let Ok(label_range) = hcl_span_to_lsp_range(rope, label_span) else {
            continue;
        };
        matched = Some((idx, spec, name.to_string(), label_range));
        break;
    }
    let (idx, spec, name, label_range) = matched?;

    // Gate check.
    let mut provider_constraint_cache: FxHashMap<(PathBuf, &'static str), Option<String>> =
        FxHashMap::default();
    let supported = compute_supported_specs(state, &module_dir, &mut provider_constraint_cache);
    if !supported.get(idx).copied().unwrap_or(false) {
        return None;
    }

    let to = resolved_to(spec);

    // Build the per-block edit set:
    //   1. Single label rewrite for this block.
    //   2. Reference rewrites limited to `<from>.<name>` (drop refs to
    //      other instances of the same `<from>` type).
    let label_rewrite = TextEdit {
        range: label_range,
        new_text: format!("\"{to}\""),
    };

    let mut by_from: FxHashMap<&'static str, (usize, &BlockRenameSpec)> = FxHashMap::default();
    by_from.insert(spec.from, (idx, spec));
    let all_refs = scan_ref_rewrites(body, rope, &by_from, &supported);
    let name_filtered_refs = filter_refs_by_name(body, rope, &all_refs, spec.from, &name);

    let mut rewrites: FxHashMap<Url, Vec<TextEdit>> = FxHashMap::default();
    let mut doc_edits = vec![label_rewrite];
    doc_edits.extend(name_filtered_refs.into_iter().map(|(_, e)| e));
    rewrites.insert(uri.clone(), doc_edits);

    // moved.tf entry per the spec's StateMigration kind.
    let module_admits_terraform_1_8 =
        module_admits_terraform_at_least(state, &module_dir, "1.8.0");
    let pending_kind = match spec.state_migration {
        StateMigration::Aliased => Some(PendingKind::Real),
        StateMigration::RequiresTerraform18 => Some(if module_admits_terraform_1_8 {
            PendingKind::Real
        } else {
            PendingKind::Commented(CommentReason::NeedsTerraform18)
        }),
        StateMigration::Manual => Some(PendingKind::Commented(CommentReason::ManualMigration)),
    };
    let mut renames_by_module: FxHashMap<PathBuf, Vec<(usize, String, PendingKind)>> =
        FxHashMap::default();
    if let Some(kind) = pending_kind {
        if spec.is_resource() {
            renames_by_module.insert(module_dir, vec![(idx, name.clone(), kind)]);
        }
    }

    let workspace_edit = build_workspace_edit(state, rewrites, renames_by_module);

    Some(CodeAction {
        title: format!("Convert {}.{name} to {to}", spec.from),
        kind: Some(lsp_types::CodeActionKind::QUICKFIX),
        diagnostics: None,
        edit: Some(workspace_edit),
        is_preferred: Some(true),
        ..Default::default()
    })
}

/// Filter ref-rewrite edits (head-ident swaps from
/// `scan_ref_rewrites`) to those whose matching traversal's
/// resource-name accessor (the first GetAttr after the head)
/// equals `target_name`. Re-walks the body alongside the edits
/// to recover names; cheap because the bodies are typically
/// small relative to ref-edit count.
fn filter_refs_by_name(
    body: &Body,
    rope: &Rope,
    all_refs: &[(usize, TextEdit)],
    from_type: &str,
    target_name: &str,
) -> Vec<(usize, TextEdit)> {
    use hcl_edit::repr::Span as _;
    // Build a position → name map by walking traversals.
    let mut name_by_pos: FxHashMap<(u32, u32), String> = FxHashMap::default();
    walk_expressions(body, &mut |expr| {
        let Expression::Traversal(t) = expr else { return };
        let Expression::Variable(v) = &t.expr else { return };
        if v.as_str() != from_type {
            return;
        }
        let Some(name) = t.operators.iter().find_map(|op| match op.value() {
            TraversalOperator::GetAttr(ident) => Some(ident.as_str().to_string()),
            _ => None,
        }) else {
            return;
        };
        let Some(span) = v.span() else { return };
        if let Ok(start) = byte_offset_to_lsp_position(rope, span.start) {
            name_by_pos.insert((start.line, start.character), name);
        }
    });
    all_refs
        .iter()
        .filter(|(_, edit)| {
            name_by_pos
                .get(&(edit.range.start.line, edit.range.start.character))
                .map(|n| n == target_name)
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// Multi-scope emit driving the generic block-rename action.
/// Called from `code_action()` once per invocation. Caller
/// supplies the per-call cache for module-level provider
/// constraint strings (one extraction per provider per call,
/// regardless of how many specs use that provider).
pub fn emit_block_rename_actions(
    state: &StateStore,
    primary_uri: &Url,
    selection: Option<Range>,
    actions: &mut Vec<CodeActionOrCommand>,
) {
    let mut scopes: Vec<Scope> = Vec::new();
    if let Some(range) = selection {
        scopes.push(Scope::Selection { range });
    }
    scopes.extend([Scope::File, Scope::Module, Scope::Workspace]);

    // Per-call cache: provider name → joined constraint (None = no
    // constraint declared). One extraction per (provider, module);
    // each spec consults its own provider's entry.
    let mut provider_constraint_cache: FxHashMap<(PathBuf, &'static str), Option<String>> =
        FxHashMap::default();

    // Two indices over the spec table (built once per call):
    // - `by_kind_label` for block-label scans (`<block_kind> "<from>"`)
    // - `by_from` for reference-traversal scans (head ident only)
    // Both store `(spec_index, &spec)` so callers can walk a body
    // once with O(1) lookup per match instead of linear-scanning
    // the 26-spec table per traversal.
    let by_kind_label: FxHashMap<(&'static str, &'static str), (usize, &BlockRenameSpec)> =
        ALL_BLOCK_RENAMES
            .iter()
            .enumerate()
            .map(|(i, s)| ((s.block_kind, s.from), (i, s)))
            .collect();
    let by_from: FxHashMap<&'static str, (usize, &BlockRenameSpec)> = ALL_BLOCK_RENAMES
        .iter()
        .enumerate()
        .map(|(i, s)| (s.from, (i, s)))
        .collect();

    // Per-call per-doc scan cache. Each scope iteration used to
    // re-walk the same body four times; with this cache the
    // walks happen once per doc per code-action call regardless
    // of scope count. Stores raw (block-rewrite triples,
    // ref-rewrite pairs) so per-scope filtering (selection
    // range, etc.) runs against cached output without re-walking.
    type ScanRow = (Vec<(usize, String, TextEdit)>, Vec<(usize, TextEdit)>);
    let mut scan_cache: FxHashMap<Url, ScanRow> = FxHashMap::default();

    for scope in scopes {
        // Per-scope: collect (uri, edits) + (module_dir, name list per spec).
        let mut edits_by_uri: FxHashMap<Url, Vec<TextEdit>> = FxHashMap::default();
        // Per-module per-spec converted entries for the
        // moved.tf builder. Tuple: (spec_index, name, pending_kind).
        // PendingKind partitions into real `moved` blocks vs
        // commented-out scaffolding the user vets manually.
        let mut renames_by_module: FxHashMap<PathBuf, Vec<(usize, String, PendingKind)>> =
            FxHashMap::default();
        let mut total_blocks = 0usize;

        for_each_doc_in_scope(state, primary_uri, scope, |doc_uri, doc| {
            let Some(body) = doc.parsed.body.as_ref() else {
                return;
            };
            let Some(module_dir) = crate::handlers::util::parent_dir(doc_uri) else {
                return;
            };

            // Per-doc scan: which specs are gate-supported in
            // this module? Cache provider constraints once per
            // (module, provider).
            let supported = compute_supported_specs(
                state,
                &module_dir,
                &mut provider_constraint_cache,
            );

            // 1+2. Compute (or fetch from cache) the per-doc
            // block + ref scans. `supported` is module-derived
            // and stable across scopes for a given doc, so the
            // cache key is the URI alone.
            if !scan_cache.contains_key(doc_uri) {
                let blocks = scan_block_rewrites(
                    body,
                    &doc.rope,
                    &by_kind_label,
                    &supported,
                );
                let refs = scan_ref_rewrites(body, &doc.rope, &by_from, &supported);
                scan_cache.insert(doc_uri.clone(), (blocks, refs));
            }
            let Some((cached_blocks, cached_refs)) = scan_cache.get(doc_uri) else {
                return;
            };
            let mut blocks = cached_blocks.clone();
            let refs = cached_refs.clone();

            // Selection scope filter.
            if let Scope::Selection { range } = scope {
                blocks.retain(|(_, _, e)| range_intersects(&e.range, &range));
            }

            if blocks.is_empty() && refs.is_empty() {
                return;
            }

            // Classify each converted block by safety: real
            // `moved` blocks for Aliased / Terraform-18-admitted
            // RequiresTerraform18; commented-out `moved` blocks
            // (with a verify-before-uncommenting header) for
            // Manual and not-yet-eligible RequiresTerraform18.
            // The commented form gives users a breadcrumb to the
            // exact `moved {}` syntax they can adopt after
            // verifying with `terraform plan` — much friendlier
            // than silently leaving them to author it from
            // scratch.
            let module_admits_terraform_1_8 =
                module_admits_terraform_at_least(state, &module_dir, "1.8.0");
            for (idx, name, _edit) in &blocks {
                let Some(spec) = ALL_BLOCK_RENAMES.get(*idx) else {
                    continue;
                };
                if !spec.is_resource() {
                    continue;
                }
                let pending_kind = match spec.state_migration {
                    StateMigration::Aliased => PendingKind::Real,
                    StateMigration::RequiresTerraform18 => {
                        if module_admits_terraform_1_8 {
                            PendingKind::Real
                        } else {
                            PendingKind::Commented(CommentReason::NeedsTerraform18)
                        }
                    }
                    StateMigration::Manual => {
                        PendingKind::Commented(CommentReason::ManualMigration)
                    }
                };
                renames_by_module
                    .entry(module_dir.clone())
                    .or_default()
                    .push((*idx, name.clone(), pending_kind));
            }
            total_blocks += blocks.len();

            // Merge edits for this doc.
            let mut doc_edits: Vec<TextEdit> =
                blocks.into_iter().map(|(_, _, e)| e).collect();
            let mut filtered_refs: Vec<TextEdit> =
                refs.into_iter().map(|(_, e)| e).collect();
            if let Scope::Selection { range } = scope {
                filtered_refs.retain(|e| range_intersects(&e.range, &range));
            }
            doc_edits.extend(filtered_refs);
            if !doc_edits.is_empty() {
                edits_by_uri
                    .entry(doc_uri.clone())
                    .or_default()
                    .extend(doc_edits);
            }
        });

        if edits_by_uri.is_empty() && renames_by_module.is_empty() {
            continue;
        }
        if total_blocks == 0 {
            continue;
        }

        let plural = if total_blocks == 1 { "" } else { "s" };
        let where_ = match scope {
            Scope::Selection { .. } => "selection",
            Scope::File => "this file",
            Scope::Module => "this module",
            Scope::Workspace => "workspace",
            Scope::Instance => continue,
        };
        let title =
            format!("Rename {total_blocks} deprecated provider type{plural} in {where_}");

        let workspace_edit =
            build_workspace_edit(state, edits_by_uri, renames_by_module);
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title,
            kind: Some(scope_kind(scope, "rename-deprecated-provider-types")),
            edit: Some(workspace_edit),
            ..Default::default()
        }));
    }
}

/// Compute which specs are currently gate-supported for the
/// given module dir. Caches per (module, provider) so a sweep
/// over 26 specs across 4 providers walks siblings 4 times max.
fn compute_supported_specs(
    state: &StateStore,
    module_dir: &std::path::Path,
    cache: &mut FxHashMap<(PathBuf, &'static str), Option<String>>,
) -> Vec<bool> {
    let mut supported = vec![false; ALL_BLOCK_RENAMES.len()];
    for (i, spec) in ALL_BLOCK_RENAMES.iter().enumerate() {
        let key = (module_dir.to_path_buf(), spec.gate_provider);
        let constraint = cache
            .entry(key)
            .or_insert_with(|| {
                module_constraint_for_provider_dir(state, module_dir, spec.gate_provider)
            })
            .clone();
        let admits = match constraint {
            None => true, // absence of evidence — fire
            Some(c) => {
                // Use tfls_diag's helpers indirectly via
                // building a minimal DeprecationRule with the
                // matching gate.
                let parsed = tfls_core::version_constraint::parse(&c);
                if parsed.constraints.is_empty() {
                    true
                } else if let Some(min) =
                    tfls_core::version_constraint::min_admitted_version(&parsed.constraints)
                {
                    tfls_core::version_constraint::version_at_least(min, spec.gate_threshold)
                } else {
                    false
                }
            }
        };
        supported[i] = admits;
    }
    supported
}

/// True when the module's aggregated `terraform { required_version }`
/// admits a Terraform CLI version at or above `floor`. Used by
/// `RequiresTerraform18` to gate cross-type `moved` emission —
/// `moved` between *different* resource types needs CLI 1.8+
/// regardless of provider support, so we must not emit
/// otherwise.
fn module_admits_terraform_at_least(
    state: &StateStore,
    module_dir: &std::path::Path,
    floor: &str,
) -> bool {
    let mut fragments: Vec<String> = Vec::new();
    for entry in state.documents.iter() {
        let uri = entry.key();
        let Ok(path) = uri.to_file_path() else { continue };
        if path.parent() != Some(module_dir) {
            continue;
        }
        let doc = entry.value();
        let Some(body) = doc.parsed.body.as_ref() else {
            continue;
        };
        if let Some(s) = tfls_diag::extract_required_version(body) {
            fragments.push(s);
        }
    }
    if fragments.is_empty() {
        // Absence of evidence — be conservative and DO NOT
        // assume the user is on 1.8+. Without a constraint we
        // can't promise the cross-type `moved` will work, so
        // skip the auto-emit.
        return false;
    }
    let joined = fragments.join(", ");
    let parsed = tfls_core::version_constraint::parse(&joined);
    if parsed.constraints.is_empty() {
        return false;
    }
    let Some(min) = tfls_core::version_constraint::min_admitted_version(&parsed.constraints)
    else {
        return false;
    };
    tfls_core::version_constraint::version_at_least(min, floor)
}

/// Module-aware constraint extraction by directory. Mirrors
/// `crate::handlers::util::module_constraint_for_provider` but
/// keyed by directory rather than URI for the per-doc gate
/// path.
fn module_constraint_for_provider_dir(
    state: &StateStore,
    module_dir: &std::path::Path,
    provider_name: &str,
) -> Option<String> {
    let mut fragments: Vec<String> = Vec::new();
    for entry in state.documents.iter() {
        let uri = entry.key();
        let Ok(path) = uri.to_file_path() else { continue };
        if path.parent() != Some(module_dir) {
            continue;
        }
        let doc = entry.value();
        let Some(body) = doc.parsed.body.as_ref() else {
            continue;
        };
        if let Some(s) = tfls_diag::extract_required_provider_version(body, provider_name) {
            fragments.push(s);
        }
    }
    if fragments.is_empty() {
        None
    } else {
        Some(fragments.join(", "))
    }
}

/// Scan the body for resource/data blocks whose label matches
/// any supported rename spec. Emits `(spec_index, name,
/// label-rewrite edit)` triples — the name comes from the
/// block's second label (`labels.get(1)`), captured here so
/// the moved.tf builder doesn't need to re-walk the body.
fn scan_block_rewrites(
    body: &Body,
    rope: &Rope,
    by_kind_label: &FxHashMap<(&'static str, &'static str), (usize, &BlockRenameSpec)>,
    supported: &[bool],
) -> Vec<(usize, String, TextEdit)> {
    use hcl_edit::repr::Span as _;

    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        let kind = block.ident.as_str();
        let Some(label) = block.labels.first() else {
            continue;
        };
        let Some(label_text) = label_str(label) else {
            continue;
        };
        let Some(&(idx, spec)) = by_kind_label.get(&(kind, label_text)) else {
            continue;
        };
        if !supported[idx] {
            continue;
        }
        let Some(name) = block.labels.get(1).and_then(label_str) else {
            continue;
        };
        let to = resolved_to(spec);
        let Some(span) = label.span() else { continue };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        out.push((
            idx,
            name.to_string(),
            TextEdit {
                range,
                new_text: format!("\"{to}\""),
            },
        ));
    }
    out
}

/// Walk every traversal in body. For `<from>.X[.attr]` where
/// `from` matches a supported spec, emit a head-ident rewrite
/// (`<from>` → `<to>`). The `.X[.attr]` tail stays unchanged
/// because schemas are identical between `from` and `to`.
fn scan_ref_rewrites(
    body: &Body,
    rope: &Rope,
    by_from: &FxHashMap<&'static str, (usize, &BlockRenameSpec)>,
    supported: &[bool],
) -> Vec<(usize, TextEdit)> {
    let mut out: Vec<(usize, TextEdit)> = Vec::new();
    walk_expressions(body, &mut |expr| {
        let Expression::Traversal(t) = expr else { return };
        let Expression::Variable(v) = &t.expr else { return };
        let head = v.as_str();
        // O(1) lookup vs the previous linear scan over the
        // 26-spec table per traversal.
        let Some(&(idx, spec)) = by_from.get(head) else { return };
        if !supported[idx] {
            return;
        }
        // The first GetAttr is the resource name (we don't
        // verify it — any traversal headed by `<from>` is a
        // candidate). Skip if no operators (e.g. bare reference).
        if !t
            .operators
            .iter()
            .any(|op| matches!(op.value(), TraversalOperator::GetAttr(_)))
        {
            return;
        }
        let Some(span) = v.span() else { return };
        if let (Ok(start), Ok(end)) = (
            byte_offset_to_lsp_position(rope, span.start),
            byte_offset_to_lsp_position(rope, span.end),
        ) {
            out.push((
                idx,
                TextEdit {
                    range: Range { start, end },
                    new_text: resolved_to(spec),
                },
            ));
        }
    });
    out
}

/// Build the WorkspaceEdit incl. moved.tf operations. Splits
/// converted entries into real `moved {}` blocks (safe to
/// auto-emit) and commented-out scaffolding (user must verify
/// before uncommenting).
fn build_workspace_edit(
    state: &StateStore,
    rewrites: FxHashMap<Url, Vec<TextEdit>>,
    renames_by_module: FxHashMap<PathBuf, Vec<(usize, String, PendingKind)>>,
) -> WorkspaceEdit {
    let mut ops: Vec<DocumentChangeOperation> = Vec::new();

    for (module_dir, mut entries) in renames_by_module {
        // Sort + dedup by (spec_idx, name) — kind is derived
        // from the spec table so duplicates would collide on
        // kind anyway.
        entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        entries.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

        // Idempotency: existing real `moved` blocks (parsed)
        // for the real-emit path; raw moved.tf text for the
        // commented-emit path so we can do a substring search
        // for already-present comment scaffolding.
        let existing_real = collect_existing_moved_pairs(state, &module_dir);
        let existing_text = read_existing_moved_tf_text(state, &module_dir);

        let target_path = module_dir.join("moved.tf");
        let Ok(target_url) = Url::from_file_path(&target_path) else {
            continue;
        };

        // Partition entries by pending kind.
        let mut real_blocks = String::new();
        let mut commented_18: Vec<(String, String, String)> = Vec::new(); // (from_type, to_type, name)
        let mut commented_manual: Vec<(String, String, String)> = Vec::new();

        for (idx, name, kind) in &entries {
            let spec = match ALL_BLOCK_RENAMES.get(*idx) {
                Some(s) => s,
                None => continue,
            };
            let from_type = spec.from.to_string();
            let to_type = resolved_to(spec);
            match kind {
                PendingKind::Real => {
                    if existing_real.contains(&(
                        from_type.clone(),
                        to_type.clone(),
                        name.clone(),
                    )) {
                        continue;
                    }
                    real_blocks.push_str(&format!(
                        "moved {{\n  from = {from_type}.{name}\n  to   = {to_type}.{name}\n}}\n"
                    ));
                }
                PendingKind::Commented(reason) => {
                    // Substring-based dedup: does the existing
                    // moved.tf already contain `# moved {` with
                    // this exact `from = <type>.<name>` line?
                    let needle = format!("from = {from_type}.{name}");
                    if existing_text.contains(&needle) {
                        continue;
                    }
                    let triple = (from_type, to_type, name.clone());
                    match reason {
                        CommentReason::NeedsTerraform18 => commented_18.push(triple),
                        CommentReason::ManualMigration => commented_manual.push(triple),
                    }
                }
            }
        }

        if real_blocks.is_empty() && commented_18.is_empty() && commented_manual.is_empty() {
            continue;
        }

        // Compose the appended text. Real blocks first
        // (uncontroversial), then commented sections with
        // explanatory headers.
        let mut body_text = String::new();
        body_text.push_str(&real_blocks);

        if !commented_manual.is_empty() {
            body_text.push_str(&commented_manual_header());
            for (from_t, to_t, name) in &commented_manual {
                body_text.push_str(&format_commented_moved(from_t, to_t, name));
                body_text.push('\n');
            }
        }
        if !commented_18.is_empty() {
            body_text.push_str(&commented_18_header());
            for (from_t, to_t, name) in &commented_18 {
                body_text.push_str(&format_commented_moved(from_t, to_t, name));
                body_text.push('\n');
            }
        }

        let strategy = resolve_target_strategy(state, &target_url, &target_path);
        match strategy {
            TargetFileStrategy::Loaded {
                eof,
                needs_leading_newline,
            }
            | TargetFileStrategy::OnDisk {
                eof,
                needs_leading_newline,
            } => {
                let mut text = String::new();
                if needs_leading_newline {
                    text.push('\n');
                }
                text.push('\n');
                text.push_str(&body_text);
                ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: target_url,
                        version: None,
                    },
                    edits: vec![OneOf::Left(TextEdit {
                        range: Range {
                            start: eof,
                            end: eof,
                        },
                        new_text: text,
                    })],
                }));
            }
            TargetFileStrategy::Create => {
                ops.push(DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: target_url.clone(),
                    options: Some(CreateFileOptions {
                        overwrite: Some(false),
                        ignore_if_exists: Some(true),
                    }),
                    annotation_id: None,
                })));
                ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: target_url,
                        version: None,
                    },
                    edits: vec![OneOf::Left(TextEdit {
                        range: Range {
                            start: Position::new(0, 0),
                            end: Position::new(0, 0),
                        },
                        new_text: body_text,
                    })],
                }));
            }
        }
    }

    for (uri, edits) in rewrites {
        ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
            edits: edits.into_iter().map(OneOf::Left).collect(),
        }));
    }

    WorkspaceEdit {
        document_changes: Some(DocumentChanges::Operations(ops)),
        ..Default::default()
    }
}

fn commented_manual_header() -> String {
    "\n# AUTO-GENERATED — VERIFY BEFORE UNCOMMENTING\n\
     # The renamed resource type(s) below have schema differences between the old and\n\
     # new names (or `MoveResourceState` support varies per provider version).\n\
     # tfls-rs cannot guarantee the in-place state migration is safe.\n\
     #\n\
     # Run `terraform plan` first. Then choose the right path:\n\
     #   - If plan shows no destructive changes → uncomment the matching block(s)\n\
     #     below to migrate state in place.\n\
     #   - If plan shows destroy + recreate → DO NOT uncomment. Instead either:\n\
     #       * `terraform state mv <old.address> <new.address>` per resource\n\
     #         (preserves state when schemas align), OR\n\
     #       * remove from state (`terraform state rm <old.address>`) and\n\
     #         `terraform import <new.address> <provider-specific-id>` after the\n\
     #         old resource is fully gone.\n\
     #   - For Kubernetes resources specifically, the import id is usually\n\
     #     `<namespace>/<name>` (or just `<name>` for cluster-scoped resources).\n\
     #     See the registry docs for the new resource for exact import syntax.\n\n"
        .to_string()
}

fn commented_18_header() -> String {
    "\n# CROSS-TYPE `moved` — REQUIRES TERRAFORM 1.8+\n\
     # The renames below are between *different* resource types and need\n\
     # Terraform CLI 1.8 (released April 2024) to apply via `moved`. Module's\n\
     # current `required_version` constraint doesn't admit 1.8+, so the\n\
     # blocks are commented out.\n\
     #\n\
     # To migrate:\n\
     #   1. Bump `required_version` in your `terraform { }` block to admit 1.8+,\n\
     #      then uncomment the matching block(s) below, OR\n\
     #   2. Migrate manually with `terraform state mv` per resource (works on\n\
     #      any Terraform version — the new resource must have an identical\n\
     #      schema, which is the case for these renames).\n\n"
        .to_string()
}

fn format_commented_moved(from_type: &str, to_type: &str, name: &str) -> String {
    format!(
        "# moved {{\n#   from = {from_type}.{name}\n#   to   = {to_type}.{name}\n# }}\n"
    )
}

/// Read the raw text of the module's `moved.tf` (loaded
/// document, on-disk file, or empty if neither exists). Used by
/// the commented-block dedup — we substring-search rather than
/// HCL-parse since comments are invisible to the parser.
fn read_existing_moved_tf_text(
    state: &StateStore,
    module_dir: &std::path::Path,
) -> String {
    let target_path = module_dir.join("moved.tf");
    let Ok(target_url) = Url::from_file_path(&target_path) else {
        return String::new();
    };
    if let Some(doc) = state.documents.get(&target_url) {
        return doc.rope.to_string();
    }
    std::fs::read_to_string(&target_path).unwrap_or_default()
}

/// `(from_type, to_type, name)` triples already covered by
/// existing `moved {}` blocks in the module — used for
/// idempotency.
fn collect_existing_moved_pairs(
    state: &StateStore,
    module_dir: &std::path::Path,
) -> FxHashSet<(String, String, String)> {
    let mut out = FxHashSet::default();
    for entry in state.documents.iter() {
        let uri = entry.key();
        let Ok(path) = uri.to_file_path() else { continue };
        if path.parent() != Some(module_dir) {
            continue;
        }
        let doc = entry.value();
        let Some(body) = doc.parsed.body.as_ref() else {
            continue;
        };
        for structure in body.iter() {
            let Some(block) = structure.as_block() else {
                continue;
            };
            if block.ident.as_str() != "moved" {
                continue;
            }
            let from = traversal_attr_string(&block.body, "from");
            let to = traversal_attr_string(&block.body, "to");
            let (Some(from), Some(to)) = (from, to) else {
                continue;
            };
            // from / to are `<type>.<name>` strings.
            let Some((from_type, from_name)) = from.split_once('.') else {
                continue;
            };
            let Some((to_type, _to_name)) = to.split_once('.') else {
                continue;
            };
            out.insert((
                from_type.to_string(),
                to_type.to_string(),
                from_name.to_string(),
            ));
        }
    }
    out
}

fn traversal_attr_string(body: &Body, key: &str) -> Option<String> {
    use hcl_edit::expr::Expression as Ex;
    for sub in body.iter() {
        let Some(attr) = sub.as_attribute() else {
            continue;
        };
        if attr.key.as_str() != key {
            continue;
        }
        if let Ex::Traversal(t) = &attr.value {
            let head = match &t.expr {
                Ex::Variable(v) => v.as_str().to_string(),
                _ => return None,
            };
            let mut acc = head;
            for op in t.operators.iter() {
                match op.value() {
                    TraversalOperator::GetAttr(d) => {
                        acc.push('.');
                        acc.push_str(d.as_str());
                    }
                    _ => return None,
                }
            }
            return Some(acc);
        }
    }
    None
}

fn label_str(label: &BlockLabel) -> Option<&str> {
    match label {
        BlockLabel::String(s) => Some(s.value().as_str()),
        BlockLabel::Ident(i) => Some(i.as_str()),
    }
}

/// How a single converted block should be represented in the
/// generated `moved.tf`. Real entries are valid `moved {}`
/// blocks Terraform applies. Commented entries are HCL
/// comments — Terraform ignores them, but the user sees the
/// exact `moved {}` syntax to uncomment after verifying the
/// migration is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Real,
    Commented(CommentReason),
}

/// Why a `moved` entry was emitted as a comment instead of a
/// real block — drives which header explains the situation in
/// `moved.tf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentReason {
    /// Cross-type `moved` requires Terraform CLI 1.8+; module
    /// constraint doesn't admit it. User can either upgrade
    /// their `required_version` or migrate state manually
    /// (`terraform state mv` / `terraform import`).
    NeedsTerraform18,
    /// Schema differences between the renamed types. The
    /// `MoveResourceState` path may or may not exist for this
    /// resource at the user's provider version. User must
    /// verify via `terraform plan` and choose the right path
    /// (uncomment, or use `terraform state mv` / `import`).
    ManualMigration,
}

/// Mirror of the strategy enum used by `code_action.rs` —
/// duplicated rather than imported because making the original
/// `pub(crate)` would expose unrelated internals. The two
/// stay narrow + structurally identical.
#[derive(Debug, Clone, Copy)]
enum TargetFileStrategy {
    Loaded {
        eof: Position,
        needs_leading_newline: bool,
    },
    OnDisk {
        eof: Position,
        needs_leading_newline: bool,
    },
    Create,
}

fn resolve_target_strategy(
    state: &StateStore,
    target_url: &Url,
    target_path: &std::path::Path,
) -> TargetFileStrategy {
    if let Some(doc) = state.documents.get(target_url) {
        let total = doc.rope.len_bytes();
        let last_char = if total == 0 {
            None
        } else {
            doc.rope
                .byte_slice(total - 1..total)
                .to_string()
                .chars()
                .next()
        };
        let needs_leading_newline = total > 0 && last_char != Some('\n');
        let eof = byte_offset_to_lsp_position(&doc.rope, total).unwrap_or(Position::new(0, 0));
        return TargetFileStrategy::Loaded {
            eof,
            needs_leading_newline,
        };
    }
    let Ok(content) = std::fs::read_to_string(target_path) else {
        return TargetFileStrategy::Create;
    };
    let rope = Rope::from_str(&content);
    let total = rope.len_bytes();
    let needs_leading_newline = total > 0 && !content.ends_with('\n');
    let eof = byte_offset_to_lsp_position(&rope, total).unwrap_or(Position::new(0, 0));
    TargetFileStrategy::OnDisk {
        eof,
        needs_leading_newline,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Every `kubernetes_*` spec must produce a `_v1`
    /// resolved-to name. Other specs spell theirs out
    /// explicitly. Catches typos in `k8s_v1` calls.
    #[test]
    fn k8s_specs_resolve_to_v1_suffix() {
        for spec in ALL_BLOCK_RENAMES {
            if !spec.from.starts_with("kubernetes_") {
                continue;
            }
            let to = resolved_to(spec);
            assert_eq!(to, format!("{}_v1", spec.from), "spec {spec:?}");
        }
    }

    /// Every AWS spec spells out a non-empty `to` field.
    #[test]
    fn aws_specs_have_explicit_to() {
        for spec in ALL_BLOCK_RENAMES {
            if !spec.from.starts_with("aws_") {
                continue;
            }
            assert!(!spec.to.is_empty(), "spec {spec:?} missing `to`");
            let to = resolved_to(spec);
            assert_eq!(to, spec.to);
        }
    }

    /// Every spec's `(block_kind, from)` should also appear in
    /// `tfls_diag::HARDCODED_DEPRECATION_LABELS` — symmetric
    /// with the diagnostic side. Catches code-action additions
    /// without paired diagnostic-suppression entries.
    #[test]
    fn specs_match_hardcoded_deprecation_labels() {
        for spec in ALL_BLOCK_RENAMES {
            assert!(
                tfls_diag::is_hardcoded_deprecation(spec.block_kind, spec.from),
                "spec for `{}.{}` not in HARDCODED_DEPRECATION_LABELS",
                spec.block_kind,
                spec.from,
            );
        }
    }

    /// Every spec is a `resource` (data sources don't have
    /// state to migrate; that family would warrant its own
    /// data-rename action with no `moved` emit).
    #[test]
    fn every_spec_is_a_resource() {
        for spec in ALL_BLOCK_RENAMES {
            assert_eq!(spec.block_kind, "resource", "spec {spec:?}");
            assert!(spec.is_resource(), "spec {spec:?}");
        }
    }

    /// AWS alb family must be `Aliased` (true aliases in the
    /// AWS provider source — same `ResourceLb()` for both
    /// names, state addresses interchangeable).
    #[test]
    fn aws_alb_family_is_aliased() {
        for spec in ALL_BLOCK_RENAMES {
            if spec.from.starts_with("aws_alb") {
                assert_eq!(
                    spec.state_migration,
                    StateMigration::Aliased,
                    "aws_alb family must be Aliased; got {spec:?}",
                );
            }
        }
    }

    /// Cross-type AWS rename (`aws_s3_bucket_object` →
    /// `aws_s3_object`) is `RequiresTerraform18` — needs CLI
    /// 1.8+ for cross-type `moved`.
    #[test]
    fn aws_s3_bucket_object_requires_terraform_18() {
        let spec = ALL_BLOCK_RENAMES
            .iter()
            .find(|s| s.from == "aws_s3_bucket_object")
            .expect("spec present");
        assert_eq!(spec.state_migration, StateMigration::RequiresTerraform18);
    }

    /// Kubernetes `_v1` renames default to `Manual` — schemas
    /// diverge between unversioned and `_v1` variants and
    /// `MoveResourceState` support is per-resource per
    /// provider version. Don't auto-emit `moved` for these.
    #[test]
    fn kubernetes_renames_are_manual() {
        for spec in ALL_BLOCK_RENAMES {
            if spec.from.starts_with("kubernetes_") {
                assert_eq!(
                    spec.state_migration,
                    StateMigration::Manual,
                    "kubernetes rename must be Manual; got {spec:?}",
                );
            }
        }
    }
}
