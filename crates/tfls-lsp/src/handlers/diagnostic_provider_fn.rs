//! Diagnostics for Terraform 1.8+ provider-defined function calls
//! (`provider::LOCAL::fn(...)`). Two failure modes:
//!
//! 1. **Unknown local** — `LOCAL` has no entry in any `terraform {
//!    required_providers { ... } }` block in the active doc or its
//!    sibling `.tf` files. Emitted as ERROR — invalid Terraform.
//! 2. **Unknown function** — `LOCAL` resolves to a provider that has
//!    at least one function in `state.functions` (so we know the
//!    plugin schema fetch succeeded), but `fn` isn't one of them.
//!    Emitted as WARNING (could be a typo, could be a yet-to-load
//!    provider — the suffix-match keeps false positives low).
//!
//! Lives in `tfls-lsp` (not `tfls-diag`) because it needs `StateStore`
//! access to look up `required_providers` across peer files AND
//! consult `state.functions`.

use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url};
use tfls_state::{DocumentState, StateStore};

use crate::handlers::completion::required_providers_local_to_name_pub;
use crate::handlers::util::parent_dir;

/// Source label used on every diagnostic this module emits — matches
/// the convention used by other tfls-lsp diagnostic helpers
/// (`schema_validation`, `deprecated_*`).
const SOURCE: &str = "terraform-ls-rs";

pub fn provider_function_call_diagnostics(
    state: &StateStore,
    uri: &Url,
    doc: &DocumentState,
) -> Vec<Diagnostic> {
    let text = doc.rope.to_string();
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let needle = b"provider::";
    let mut search_from = 0usize;
    while search_from + needle.len() <= bytes.len() {
        let Some(rel) = find_subslice(&bytes[search_from..], needle) else {
            break;
        };
        let kw_start = search_from + rel;
        // Identifier-boundary on the left: `provider` must not be the
        // tail of a longer identifier.
        if kw_start > 0 {
            let prev = bytes[kw_start - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                search_from = kw_start + needle.len();
                continue;
            }
        }
        let after_provider = kw_start + needle.len();
        // Parse `<local>::<fn>` with identifier rules.
        let local_start = after_provider;
        let mut p = local_start;
        while p < bytes.len() && (bytes[p].is_ascii_alphanumeric() || bytes[p] == b'_') {
            p += 1;
        }
        let local_end = p;
        if local_end == local_start
            || p + 1 >= bytes.len()
            || bytes[p] != b':'
            || bytes[p + 1] != b':'
        {
            search_from = after_provider;
            continue;
        }
        let fn_start = p + 2;
        let mut q = fn_start;
        while q < bytes.len() && (bytes[q].is_ascii_alphanumeric() || bytes[q] == b'_') {
            q += 1;
        }
        let fn_end = q;
        if fn_end == fn_start {
            search_from = fn_start;
            continue;
        }
        // Must be followed by `(` for a call (skip whitespace).
        let mut r = fn_end;
        while r < bytes.len() && (bytes[r] == b' ' || bytes[r] == b'\t') {
            r += 1;
        }
        if r >= bytes.len() || bytes[r] != b'(' {
            search_from = fn_end;
            continue;
        }
        let local = &text[local_start..local_end];
        let fn_name = &text[fn_start..fn_end];
        let provider_name = lookup_local(state, uri, local);
        let range_start = byte_to_pos(&doc.rope, kw_start);
        let range_end = byte_to_pos(&doc.rope, fn_end);
        if let (Some(start), Some(end)) = (range_start, range_end) {
            let range = Range { start, end };
            match provider_name {
                None => {
                    out.push(Diagnostic {
                        range,
                        severity: Some(DiagnosticSeverity::ERROR),
                        source: Some(SOURCE.into()),
                        message: format!(
                            "Unknown provider local name `{local}` — no entry in `terraform {{ required_providers {{ … }} }}`"
                        ),
                        ..Default::default()
                    });
                }
                Some(provider) => {
                    if !provider_has_any_function(state, &provider) {
                        // Schema/functions probably not fetched for
                        // this provider yet; skip rather than spam.
                        search_from = fn_end;
                        continue;
                    }
                    if !provider_has_function(state, &provider, fn_name) {
                        out.push(Diagnostic {
                            range,
                            severity: Some(DiagnosticSeverity::WARNING),
                            source: Some(SOURCE.into()),
                            message: format!(
                                "Provider `{provider}` does not expose a function `{fn_name}`"
                            ),
                            ..Default::default()
                        });
                    }
                }
            }
        }
        search_from = fn_end;
    }
    out
}

fn lookup_local(state: &StateStore, uri: &Url, local: &str) -> Option<String> {
    if let Some(doc) = state.documents.get(uri) {
        if let Some(body) = doc.parsed.body.as_ref() {
            if let Some(name) = required_providers_local_to_name_pub(body, local) {
                return Some(name);
            }
        }
    }
    let target_dir = parent_dir(uri)?;
    for entry in state.documents.iter() {
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
        if let Some(name) = required_providers_local_to_name_pub(body, local) {
            return Some(name);
        }
    }
    None
}

fn provider_has_any_function(state: &StateStore, provider: &str) -> bool {
    let needle = format!("::{provider}::");
    state
        .functions
        .iter()
        .any(|e| e.key().starts_with("provider::") && e.key().contains(&needle))
}

fn provider_has_function(state: &StateStore, provider: &str, fn_name: &str) -> bool {
    let suffix = format!("::{provider}::{fn_name}");
    state
        .functions
        .iter()
        .any(|e| e.key().starts_with("provider::") && e.key().ends_with(&suffix))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn byte_to_pos(rope: &ropey::Rope, byte: usize) -> Option<Position> {
    tfls_parser::byte_offset_to_lsp_position(rope, byte).ok()
}
