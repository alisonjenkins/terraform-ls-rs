//! Scope abstraction for multi-target code actions.
//!
//! Most code actions are meaningful at more than one scope:
//! the user might want to fix a single occurrence under the
//! cursor, every occurrence in the active file, every
//! occurrence in the same module dir, or every occurrence
//! anywhere in the workspace. They might also have a visual
//! selection and want to constrain to that range.
//!
//! Hand-coding the per-scope iteration in every action ends in
//! drift — the file scope picker handles `state.documents.get`,
//! the module scope re-implements `parent_dir` filtering, the
//! workspace scope iterates `state.documents`, and each adds
//! its own title formatting. This module centralises:
//!
//! - The [`Scope`] enum (the five scopes).
//! - [`for_each_doc_in_scope`] — single iteration callback,
//!   covers all scopes uniformly.
//! - [`build_scoped_action`] — assembles a scope-tagged
//!   `CodeAction` from a per-uri edit map; suppresses the menu
//!   entry when the map is empty.
//! - [`scope_title`] / [`scope_kind`] — standardised user-facing
//!   strings + LSP `CodeActionKind` per scope.
//!
//! Add a new scoped action by writing a single per-doc scan
//! function `(uri, body, rope) -> Vec<TextEdit>` and looping it
//! over `for_each_doc_in_scope` — see `code_action.rs` for
//! examples.

use std::collections::HashMap;

use lsp_types::{
    CodeAction, CodeActionKind, Diagnostic, Range, TextEdit, Url, WorkspaceEdit,
};
use tfls_state::{DocumentState, StateStore};

use crate::handlers::util::parent_dir;

/// Where a code action's edits should fall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Single thing under the cursor (or attached to one
    /// specific diagnostic). The handler picks the actual edit;
    /// this scope is mostly a marker for title / kind purposes.
    Instance,
    /// User's highlighted range. The per-doc scan should filter
    /// edits whose `Range` does NOT intersect `range`.
    Selection { range: Range },
    /// Active document only.
    File,
    /// Every doc whose parent directory matches the active
    /// document's parent — a Terraform module is a directory.
    Module,
    /// Every indexed `.tf` document, skipping `.terraform/*`.
    Workspace,
}

impl Scope {
    fn human_label(self) -> &'static str {
        match self {
            Scope::Instance => "instance",
            Scope::Selection { .. } => "selection",
            Scope::File => "this file",
            Scope::Module => "this module",
            Scope::Workspace => "workspace",
        }
    }

    fn kind_suffix(self) -> Option<&'static str> {
        match self {
            Scope::Instance => None,
            Scope::Selection { .. } => Some("selection"),
            Scope::File => None, // bare `source.fixAll.<id>`
            Scope::Module => Some("module"),
            Scope::Workspace => Some("workspace"),
        }
    }
}

/// Visit every doc that falls within `scope`. Iteration is
/// callback-style so DashMap shard guards never escape this
/// function. The callback receives the doc's URI and a borrowed
/// `DocumentState`; it must NOT call back into `state`'s
/// document map (would deadlock).
///
/// Workspace iteration skips paths containing `.terraform/` to
/// match the discovery-walker convention — those files are
/// indexed for module-output resolution, not user-authored
/// content the user is editing.
pub fn for_each_doc_in_scope<F>(
    state: &StateStore,
    primary_uri: &Url,
    scope: Scope,
    mut visit: F,
) where
    F: FnMut(&Url, &DocumentState),
{
    match scope {
        Scope::Instance | Scope::Selection { .. } | Scope::File => {
            if let Some(doc) = state.documents.get(primary_uri) {
                visit(primary_uri, &doc);
            }
        }
        Scope::Module => {
            let Some(target_dir) = parent_dir(primary_uri) else { return };
            for entry in state.documents.iter() {
                let uri = entry.key();
                let Ok(path) = uri.to_file_path() else { continue };
                if path.parent() != Some(&target_dir) {
                    continue;
                }
                visit(uri, entry.value());
            }
        }
        Scope::Workspace => {
            for entry in state.documents.iter() {
                let uri = entry.key();
                if uri.path().contains("/.terraform/") {
                    continue;
                }
                visit(uri, entry.value());
            }
        }
    }
}

/// Assemble a scope-tagged `CodeAction` from a per-uri edit
/// map. Returns `None` when the map (after filtering empty
/// entries) carries no edits — callers add this to their
/// actions vec without a separate is_empty check.
pub fn build_scoped_action(
    scope: Scope,
    edits_by_uri: HashMap<Url, Vec<TextEdit>>,
    title_template: &str,
    item_label: &str,
    diagnostics: Option<Vec<Diagnostic>>,
    action_id: &str,
) -> Option<CodeAction> {
    let edits_by_uri: HashMap<Url, Vec<TextEdit>> = edits_by_uri
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect();
    if edits_by_uri.is_empty() {
        return None;
    }
    let count: usize = edits_by_uri.values().map(Vec::len).sum();
    Some(CodeAction {
        title: scope_title(title_template, item_label, scope, count),
        kind: Some(scope_kind(scope, action_id)),
        diagnostics,
        edit: Some(WorkspaceEdit {
            changes: Some(edits_by_uri),
            ..Default::default()
        }),
        is_preferred: None,
        ..Default::default()
    })
}

/// Build the standard title for a scoped action. Examples:
///
/// - `scope_title("Unwrap interpolation", "deprecated interpolation", Instance, 1)`
///   → `"Unwrap interpolation"`
/// - `scope_title("Unwrap interpolation", "deprecated interpolation", File, 5)`
///   → `"Unwrap 5 deprecated interpolations in this file"`
pub fn scope_title(
    template: &str,
    item_label: &str,
    scope: Scope,
    count: usize,
) -> String {
    if matches!(scope, Scope::Instance) {
        return template.to_string();
    }
    let plural = if count == 1 { "" } else { "s" };
    let where_ = scope.human_label();
    let leading_verb = template.split_whitespace().next().unwrap_or(template);
    // For aggregate forms keep the verb but rewrite the object:
    // "Unwrap interpolation" → "Unwrap N item_label(s) in WHERE".
    format!("{leading_verb} {count} {item_label}{plural} in {where_}")
}

/// LSP `CodeActionKind` for the given scope. Clients use these
/// strings (via `params.context.only`) to filter the menu, so
/// they should be stable + collision-free across actions.
///
/// `action_id` is a short stable identifier per action family
/// (e.g. `"unwrap-interpolation"`). It namespaces the kind so
/// two unrelated actions can't accidentally collide on the same
/// `source.fixAll.terraform-ls-rs.module` string.
pub fn scope_kind(scope: Scope, action_id: &str) -> CodeActionKind {
    let base = "terraform-ls-rs";
    let raw = match scope.kind_suffix() {
        None => match scope {
            Scope::Instance => return CodeActionKind::QUICKFIX,
            Scope::File => format!("source.fixAll.{base}.{action_id}"),
            _ => unreachable!(),
        },
        Some(suffix) => match scope {
            Scope::Selection { .. } => format!("quickfix.{base}.{action_id}.{suffix}"),
            Scope::Module | Scope::Workspace => {
                format!("source.fixAll.{base}.{action_id}.{suffix}")
            }
            _ => unreachable!(),
        },
    };
    CodeActionKind::from(raw)
}

/// True when two ranges share at least one position.
pub fn range_intersects(a: &Range, b: &Range) -> bool {
    let a_start = (a.start.line, a.start.character);
    let a_end = (a.end.line, a.end.character);
    let b_start = (b.start.line, b.start.character);
    let b_end = (b.end.line, b.end.character);
    !(a_end < b_start || b_end < a_start)
}

/// True when the LSP range is empty (start == end). Empty ranges
/// are how single-point invocations (no selection) arrive.
pub fn range_is_empty(range: &Range) -> bool {
    range.start == range.end
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::Position;

    fn r(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range {
            start: Position::new(sl, sc),
            end: Position::new(el, ec),
        }
    }

    #[test]
    fn title_instance_uses_template_unchanged() {
        let t = scope_title("Unwrap interpolation", "interp", Scope::Instance, 1);
        assert_eq!(t, "Unwrap interpolation");
    }

    #[test]
    fn title_file_pluralises() {
        let t = scope_title("Unwrap interpolation", "interp", Scope::File, 5);
        assert_eq!(t, "Unwrap 5 interps in this file");
    }

    #[test]
    fn title_module_singular() {
        let t = scope_title("Unwrap interpolation", "interp", Scope::Module, 1);
        assert_eq!(t, "Unwrap 1 interp in this module");
    }

    #[test]
    fn title_workspace_pluralises() {
        let t = scope_title("Unwrap interpolation", "interp", Scope::Workspace, 47);
        assert_eq!(t, "Unwrap 47 interps in workspace");
    }

    #[test]
    fn title_selection() {
        let t = scope_title(
            "Unwrap interpolation",
            "interp",
            Scope::Selection { range: r(0, 0, 5, 0) },
            3,
        );
        assert_eq!(t, "Unwrap 3 interps in selection");
    }

    #[test]
    fn kind_namespacing() {
        assert_eq!(
            scope_kind(Scope::Instance, "unwrap").as_str(),
            "quickfix"
        );
        assert_eq!(
            scope_kind(Scope::File, "unwrap").as_str(),
            "source.fixAll.terraform-ls-rs.unwrap"
        );
        assert_eq!(
            scope_kind(Scope::Module, "unwrap").as_str(),
            "source.fixAll.terraform-ls-rs.unwrap.module"
        );
        assert_eq!(
            scope_kind(Scope::Workspace, "unwrap").as_str(),
            "source.fixAll.terraform-ls-rs.unwrap.workspace"
        );
        assert_eq!(
            scope_kind(Scope::Selection { range: r(0, 0, 1, 0) }, "unwrap").as_str(),
            "quickfix.terraform-ls-rs.unwrap.selection"
        );
    }

    #[test]
    fn range_intersection_basics() {
        // Identical.
        assert!(range_intersects(&r(0, 0, 5, 0), &r(0, 0, 5, 0)));
        // Overlap.
        assert!(range_intersects(&r(0, 0, 5, 0), &r(3, 0, 7, 0)));
        // Touching at boundary still intersect.
        assert!(range_intersects(&r(0, 0, 5, 0), &r(5, 0, 7, 0)));
        // Disjoint.
        assert!(!range_intersects(&r(0, 0, 5, 0), &r(6, 0, 10, 0)));
    }

    #[test]
    fn build_action_returns_none_when_empty() {
        let map: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        assert!(build_scoped_action(
            Scope::File,
            map,
            "Unwrap",
            "interp",
            None,
            "unwrap",
        )
        .is_none());
    }

    #[test]
    fn build_action_drops_empty_uri_entries() {
        let mut map: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        let url = Url::parse("file:///x.tf").unwrap();
        map.insert(url, Vec::new());
        assert!(build_scoped_action(
            Scope::File,
            map,
            "Unwrap",
            "interp",
            None,
            "unwrap",
        )
        .is_none());
    }
}
