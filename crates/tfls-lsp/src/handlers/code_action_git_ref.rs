//! Code actions for git module ref pinning, paired with the
//! `module_mutable_ref` / `module_ref_tag_mismatch` / `module_outdated`
//! diagnostics:
//!
//! - **pin_ref**: mutable `?ref=<tag>` → resolve to commit SHA, add `# tag`.
//! - **add_sha_comment** (cursor): `?ref=<sha>` with no comment → reverse-resolve
//!   to the tag and add `# tag`.
//! - **mismatch**: `?ref=<sha> # tag` where the tag moved → re-pin SHA, or fix
//!   the comment.
//! - **switch_version** (diag + cursor): switch a pinned module to a newer
//!   available tag, re-pinning to its SHA.
//!
//! All resolution goes through `git ls-remote` (gated on `cliEnabled`); edits
//! preserve the source's scheme — only the `?ref=` value and `# tag` comment
//! change. Comments are emitted as raw `TextEdit`s (hcl-edit doesn't model them).

use std::collections::HashMap;

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{
    CodeAction, CodeActionKind, Diagnostic, DiagnosticSeverity, Position, Range, TextEdit,
    WorkspaceEdit,
};
use ropey::Rope;
use url::Url;

use tfls_core::git_ref::{
    looks_like_commit_sha, newer_versions, parse_version_core, tag_namespace,
};
use tfls_diag::{
    extract_ref, has_trailing_comment, is_git_source, ref_value_span, trailing_comment_tag,
};
use tfls_provider_protocol::git_refs;

use crate::backend::Backend;
use crate::handlers::code_action::contains;

// ---- matchers ----

pub fn is_mutable_ref(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::WARNING)
        && diag.source.as_deref() == Some("terraform-ls-rs")
        && diag.message.contains("pinned to mutable git ref")
}

pub fn is_ref_tag_mismatch(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::WARNING)
        && diag.source.as_deref() == Some("terraform-ls-rs")
        && diag.message.contains("does not match tag")
}

pub fn is_outdated(diag: &Diagnostic) -> bool {
    diag.severity == Some(DiagnosticSeverity::INFORMATION)
        && diag.source.as_deref() == Some("terraform-ls-rs")
        && diag.message.contains("newer module version available")
}

// ---- builders ----

/// Behavior 1: pin a mutable `?ref=` to its commit SHA, keeping the ref as a
/// trailing comment.
pub async fn pin_ref_action(
    backend: &Backend,
    uri: &Url,
    diag: &Diagnostic,
    body: &Body,
    rope: &Rope,
) -> Option<CodeAction> {
    let cli_enabled = backend.state.config.snapshot().cli_enabled;
    if !cli_enabled {
        return None;
    }
    let (value_span, raw) = find_module_source_at(body, rope, diag.range.start)?;
    let ref_name = extract_ref(&raw)?.to_string();
    let sha = git_refs::resolve_ref_to_sha(&raw, &ref_name, cli_enabled)
        .await
        .ok()?;

    let mut edits = vec![ref_replacement_edit(rope, &value_span, &raw, &sha)?];
    edits.push(comment_edit(rope, value_span.end, &ref_name)?);

    Some(quickfix(
        format!(
            "Pin git ref `{ref_name}` to commit {} (keep tag as comment)",
            short(&sha)
        ),
        uri,
        edits,
        Some(diag.clone()),
        true,
    ))
}

/// Behavior 2 (cursor): a SHA-pinned source with no comment — reverse-resolve
/// the SHA to a tag and add the comment.
pub async fn add_sha_comment_action(
    backend: &Backend,
    uri: &Url,
    pos: Position,
    body: &Body,
    rope: &Rope,
) -> Option<CodeAction> {
    let cli_enabled = backend.state.config.snapshot().cli_enabled;
    if !cli_enabled {
        return None;
    }
    let (value_span, raw) = find_module_source_at(body, rope, pos)?;
    if !is_git_source(&raw) {
        return None;
    }
    let pinned = extract_ref(&raw)?;
    if !looks_like_commit_sha(pinned) {
        return None;
    }
    if has_trailing_comment(rope, value_span.end) {
        return None;
    }
    let tags = git_refs::list_repo_tags(&raw, cli_enabled).await.ok()?;
    let tag = git_refs::sha_to_tag(&tags, pinned)?.to_string();
    let edit = comment_edit(rope, value_span.end, &tag)?;
    Some(quickfix(
        format!("Add tag comment `{tag}` for pinned commit"),
        uri,
        vec![edit],
        None,
        false,
    ))
}

/// Behavior 3: a SHA/tag mismatch — offer to re-pin the SHA to the tag's
/// current commit, or to fix the comment to the tag matching the pinned SHA.
pub async fn mismatch_actions(
    backend: &Backend,
    uri: &Url,
    diag: &Diagnostic,
    body: &Body,
    rope: &Rope,
) -> Vec<CodeAction> {
    let cli_enabled = backend.state.config.snapshot().cli_enabled;
    if !cli_enabled {
        return Vec::new();
    }
    let Some((value_span, raw)) = find_module_source_at(body, rope, diag.range.start) else {
        return Vec::new();
    };
    let Some(pinned) = extract_ref(&raw) else {
        return Vec::new();
    };
    let Some(comment_tag) = trailing_comment_tag(rope, value_span.end) else {
        return Vec::new();
    };
    let Ok(tags) = git_refs::list_repo_tags(&raw, cli_enabled).await else {
        return Vec::new();
    };

    let mut out = Vec::new();
    // (a) Update the pinned SHA to the tag's current commit.
    if let Some(sha) = git_refs::tag_to_sha(&tags, &comment_tag) {
        if let Some(edit) = ref_replacement_edit(rope, &value_span, &raw, sha) {
            out.push(quickfix(
                format!(
                    "Update pinned commit to tag `{comment_tag}` ({})",
                    short(sha)
                ),
                uri,
                vec![edit],
                Some(diag.clone()),
                false,
            ));
        }
    }
    // (b) Fix the comment to the tag that matches the pinned SHA.
    if let Some(real_tag) = git_refs::sha_to_tag(&tags, pinned) {
        if real_tag != comment_tag {
            if let Some(edit) = comment_edit(rope, value_span.end, real_tag) {
                out.push(quickfix(
                    format!("Update comment to `{real_tag}` (matches pinned commit)"),
                    uri,
                    vec![edit],
                    Some(diag.clone()),
                    false,
                ));
            }
        }
    }
    out
}

/// Behavior 5: switch the module to a newer available version (latest, plus
/// latest-within-current-major if different). Each action re-pins to that
/// tag's SHA and updates the comment.
pub async fn switch_version_actions(
    backend: &Backend,
    uri: &Url,
    pos: Position,
    body: &Body,
    rope: &Rope,
    diag: Option<&Diagnostic>,
) -> Vec<CodeAction> {
    let cli_enabled = backend.state.config.snapshot().cli_enabled;
    if !cli_enabled {
        return Vec::new();
    }
    let Some((value_span, raw)) = find_module_source_at(body, rope, pos) else {
        return Vec::new();
    };
    if !is_git_source(&raw) {
        return Vec::new();
    }
    let Some(refv) = extract_ref(&raw) else {
        return Vec::new();
    };
    // Current version: the ref if it's a tag, else the trailing comment.
    let current = if looks_like_commit_sha(refv) {
        match trailing_comment_tag(rope, value_span.end) {
            Some(t) => t,
            None => return Vec::new(),
        }
    } else {
        refv.to_string()
    };

    let Ok(tags) = git_refs::list_repo_tags(&raw, cli_enabled).await else {
        return Vec::new();
    };
    let candidates = newer_versions(&git_refs::tag_names(&tags), &current);
    if candidates.is_empty() {
        return Vec::new();
    }

    // Bound the menu: latest overall + latest within the current major.
    let mut chosen: Vec<String> = Vec::new();
    chosen.push(candidates[0].clone());
    if let Some(cur_major) = tag_namespace(&current)
        .and_then(|(_, c)| parse_version_core(c))
        .map(|v| v.major)
    {
        if let Some(in_major) = candidates.iter().find(|t| {
            tag_namespace(t)
                .and_then(|(_, c)| parse_version_core(c))
                .map(|v| v.major)
                == Some(cur_major)
        }) {
            if !chosen.contains(in_major) {
                chosen.push(in_major.clone());
            }
        }
    }

    let mut out = Vec::new();
    for tag in chosen {
        let Some(sha) = git_refs::tag_to_sha(&tags, &tag) else {
            continue;
        };
        let mut edits = Vec::new();
        let Some(re) = ref_replacement_edit(rope, &value_span, &raw, sha) else {
            continue;
        };
        edits.push(re);
        if let Some(ce) = comment_edit(rope, value_span.end, &tag) {
            edits.push(ce);
        }
        out.push(quickfix(
            format!("Update module to {tag}"),
            uri,
            edits,
            diag.cloned(),
            false,
        ));
    }
    out
}

// ---- helpers ----

/// Find the `module` block `source` string whose quoted-value span contains
/// `pos`; return `(value_span, unquoted_source)`.
fn find_module_source_at(
    body: &Body,
    rope: &Rope,
    pos: Position,
) -> Option<(std::ops::Range<usize>, String)> {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "module" {
            continue;
        }
        for attr in block.body.iter().filter_map(|s| s.as_attribute()) {
            if attr.key.as_str() != "source" {
                continue;
            }
            let Expression::String(s) = &attr.value else {
                continue;
            };
            let Some(span) = attr.value.span() else {
                continue;
            };
            let Ok(range) = tfls_parser::hcl_span_to_lsp_range(rope, span.clone()) else {
                continue;
            };
            if contains(&range, pos) {
                return Some((span, s.value().as_str().to_string()));
            }
        }
    }
    None
}

/// Replace just the ref VALUE within the quoted source string with `new_ref`.
fn ref_replacement_edit(
    rope: &Rope,
    value_span: &std::ops::Range<usize>,
    raw: &str,
    new_ref: &str,
) -> Option<TextEdit> {
    let (rs, re) = ref_value_span(raw)?;
    // value_span covers the quoted string; +1 skips the opening quote.
    let content_start = value_span.start + 1;
    let start = tfls_parser::byte_offset_to_lsp_position(rope, content_start + rs).ok()?;
    let end = tfls_parser::byte_offset_to_lsp_position(rope, content_start + re).ok()?;
    Some(TextEdit {
        range: Range { start, end },
        new_text: new_ref.to_string(),
    })
}

/// Add or replace a trailing `# <tag>` comment on the source line.
/// `after_quote_byte` is the byte just past the source string's closing quote.
fn comment_edit(rope: &Rope, after_quote_byte: usize, tag: &str) -> Option<TextEdit> {
    let line_idx = rope.try_byte_to_line(after_quote_byte).ok()?;
    let line_start = rope.try_line_to_byte(line_idx).ok()?;
    let line = rope.get_line(line_idx)?.to_string();
    let content_len = line.trim_end_matches(['\n', '\r']).len();
    let eol_byte = line_start + content_len;
    let from = after_quote_byte.saturating_sub(line_start).min(content_len);
    let tail = &line[from..content_len];
    let marker_rel = tail.find("//").or_else(|| tail.find('#'));
    let (start_byte, new_text) = match marker_rel {
        Some(m) => (line_start + from + m, format!("# {tag}")),
        None => (eol_byte, format!(" # {tag}")),
    };
    let start = tfls_parser::byte_offset_to_lsp_position(rope, start_byte).ok()?;
    let end = tfls_parser::byte_offset_to_lsp_position(rope, eol_byte).ok()?;
    Some(TextEdit {
        range: Range { start, end },
        new_text,
    })
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

#[allow(clippy::too_many_arguments)]
fn quickfix(
    title: String,
    uri: &Url,
    edits: Vec<TextEdit>,
    diag: Option<Diagnostic>,
    preferred: bool,
) -> CodeAction {
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri.clone(), edits);
    CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: diag.map(|d| vec![d]),
        edit: Some(WorkspaceEdit {
            changes: Some(tfls_core::uri::changes_to_uri(changes)),
            ..Default::default()
        }),
        is_preferred: Some(preferred),
        ..Default::default()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Build (value_span, raw) for a single-line `source = "<src>"`.
    fn span_for(line: &str) -> (Rope, std::ops::Range<usize>, String) {
        let rope = Rope::from_str(line);
        let q1 = line.find('"').unwrap();
        let q2 = line[q1 + 1..].find('"').unwrap() + q1 + 1;
        let raw = line[q1 + 1..q2].to_string();
        (rope, q1..q2 + 1, raw)
    }

    #[test]
    fn ref_replacement_targets_the_ref_token() {
        let line = "  source = \"git::ssh://h/o/r?ref=v1.2.3\"\n";
        let (rope, span, raw) = span_for(line);
        let edit = ref_replacement_edit(&rope, &span, &raw, "deadbeefcafe").unwrap();
        assert_eq!(edit.new_text, "deadbeefcafe");
        // Single line: character == byte column. The ref `v1.2.3` sits at these cols.
        let start_col = line.find("v1.2.3").unwrap() as u32;
        assert_eq!(edit.range.start.line, 0);
        assert_eq!(edit.range.start.character, start_col);
        assert_eq!(edit.range.end.character, start_col + "v1.2.3".len() as u32);
    }

    #[test]
    fn comment_added_at_eol_when_absent() {
        let line = "  source = \"git::ssh://h/o/r?ref=abc1234\"\n";
        let (rope, span, _raw) = span_for(line);
        let edit = comment_edit(&rope, span.end, "v1.2.3").unwrap();
        assert_eq!(edit.new_text, " # v1.2.3");
        // zero-width insert at end-of-line content
        assert_eq!(edit.range.start, edit.range.end);
        let eol_col = line.trim_end_matches('\n').len() as u32;
        assert_eq!(edit.range.start.character, eol_col);
    }

    #[test]
    fn comment_replaced_when_present() {
        let line = "  source = \"git::ssh://h/o/r?ref=abc1234\" # v1.0.0\n";
        let (rope, span, _raw) = span_for(line);
        let edit = comment_edit(&rope, span.end, "v2.0.0").unwrap();
        assert_eq!(edit.new_text, "# v2.0.0");
        let hash_col = line.find('#').unwrap() as u32;
        let eol_col = line.trim_end_matches('\n').len() as u32;
        assert_eq!(edit.range.start.character, hash_col);
        assert_eq!(edit.range.end.character, eol_col);
    }
}
