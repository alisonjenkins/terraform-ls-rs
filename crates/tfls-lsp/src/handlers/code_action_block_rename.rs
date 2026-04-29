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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

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
    let mut provider_constraint_cache: HashMap<(PathBuf, &'static str), Option<String>> =
        HashMap::new();

    // Group specs by `from` for fast block-walk lookup.
    let by_from: HashMap<(&'static str, &'static str), &BlockRenameSpec> = ALL_BLOCK_RENAMES
        .iter()
        .map(|s| ((s.block_kind, s.from), s))
        .collect();

    for scope in scopes {
        // Per-scope: collect (uri, edits) + (module_dir, name list per spec).
        let mut edits_by_uri: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        // Per-module per-spec converted-name lists, used by the
        // moved.tf builder. Key: (module_dir, spec_index).
        let mut renames_by_module: HashMap<PathBuf, Vec<(usize, String)>> = HashMap::new();
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

            // 1. Block label rewrites + name collection.
            // Returns `(spec_index, name, edit)` so the moved.tf
            // builder can pair each rewrite with its block name
            // without re-walking the body.
            let mut blocks = scan_block_rewrites(body, &doc.rope, &by_from, &supported);
            // 2. Reference rewrites.
            let refs = scan_ref_rewrites(body, &doc.rope, &supported);

            // Selection scope filter.
            if let Scope::Selection { range } = scope {
                blocks.retain(|(_, _, e)| range_intersects(&e.range, &range));
            }

            if blocks.is_empty() && refs.is_empty() {
                return;
            }

            // Track converted (spec_index, name) for moved.tf —
            // ONLY when the spec's state-migration kind is
            // safe to emit. `Manual` skipped entirely
            // (auto-emitted `moved` would be wrong). `RequiresTerraform18`
            // gated by the module's aggregated `required_version`
            // admitting 1.8+; below that, cross-type `moved` is
            // CLI-side-unsupported and would error.
            let module_admits_terraform_1_8 =
                module_admits_terraform_at_least(state, &module_dir, "1.8.0");
            for (idx, name, _edit) in &blocks {
                let Some(spec) = ALL_BLOCK_RENAMES.get(*idx) else {
                    continue;
                };
                if !spec.is_resource() {
                    continue;
                }
                let safe = match spec.state_migration {
                    StateMigration::Aliased => true,
                    StateMigration::RequiresTerraform18 => module_admits_terraform_1_8,
                    StateMigration::Manual => false,
                };
                if !safe {
                    continue;
                }
                renames_by_module
                    .entry(module_dir.clone())
                    .or_default()
                    .push((*idx, name.clone()));
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
    cache: &mut HashMap<(PathBuf, &'static str), Option<String>>,
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
    by_from: &HashMap<(&'static str, &'static str), &BlockRenameSpec>,
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
        let Some(spec) = by_from.get(&(kind, label_text)) else {
            continue;
        };
        let idx = spec_index(spec);
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
    supported: &[bool],
) -> Vec<(usize, TextEdit)> {
    let mut out: Vec<(usize, TextEdit)> = Vec::new();
    walk_expressions(body, &mut |expr| {
        let Expression::Traversal(t) = expr else { return };
        let Expression::Variable(v) = &t.expr else { return };
        let head = v.as_str();
        // Find a spec whose `from` matches the head.
        let Some((idx, spec)) = ALL_BLOCK_RENAMES
            .iter()
            .enumerate()
            .find(|(_, s)| s.from == head)
        else {
            return;
        };
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

/// Build the WorkspaceEdit incl. moved.tf operations. Mirrors
/// the null_resource action's pattern but parametrised over
/// the spec table.
fn build_workspace_edit(
    state: &StateStore,
    rewrites: HashMap<Url, Vec<TextEdit>>,
    renames_by_module: HashMap<PathBuf, Vec<(usize, String)>>,
) -> WorkspaceEdit {
    let mut ops: Vec<DocumentChangeOperation> = Vec::new();

    // Per-module moved.tf builder.
    for (module_dir, mut entries) in renames_by_module {
        // Dedupe by (spec_index, name).
        entries.sort();
        entries.dedup();
        // Drop entries already covered by an existing `moved`
        // block in any sibling — keeps re-runs idempotent.
        let existing = collect_existing_moved_pairs(state, &module_dir);
        let to_add: Vec<(usize, String)> = entries
            .into_iter()
            .filter(|(idx, name)| {
                let spec = match ALL_BLOCK_RENAMES.get(*idx) {
                    Some(s) => s,
                    None => return false,
                };
                let to = resolved_to(spec);
                !existing.contains(&(spec.from.to_string(), to, name.clone()))
            })
            .collect();
        if to_add.is_empty() {
            continue;
        }

        let target_path = module_dir.join("moved.tf");
        let Ok(target_url) = Url::from_file_path(&target_path) else {
            continue;
        };

        let mut body_text = String::new();
        for (idx, name) in &to_add {
            let spec = match ALL_BLOCK_RENAMES.get(*idx) {
                Some(s) => s,
                None => continue,
            };
            let to = resolved_to(spec);
            body_text.push_str(&format!(
                "moved {{\n  from = {}.{name}\n  to   = {to}.{name}\n}}\n",
                spec.from
            ));
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

    // Append rewrite edits.
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

/// `(from_type, to_type, name)` triples already covered by
/// existing `moved {}` blocks in the module — used for
/// idempotency.
fn collect_existing_moved_pairs(
    state: &StateStore,
    module_dir: &std::path::Path,
) -> HashSet<(String, String, String)> {
    let mut out = HashSet::new();
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

fn spec_index(spec: &BlockRenameSpec) -> usize {
    ALL_BLOCK_RENAMES
        .iter()
        .position(|s| std::ptr::eq(s, spec))
        .unwrap_or(0)
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
