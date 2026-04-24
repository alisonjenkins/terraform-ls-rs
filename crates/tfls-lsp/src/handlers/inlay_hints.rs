//! `textDocument/inlayHint` emitter.
//!
//! Two hint families live here:
//!
//! 1. **Variable defaults** — show the literal default value after a
//!    `var.<name>` reference when the variable block declares a
//!    literal-scalar `default`.
//! 2. **Version freshness** — for `required_version`, provider
//!    `version` inside `required_providers`, and module `version`:
//!    show whether the constraint resolves to the latest published
//!    release and, for stale exact pins, how many days old the
//!    latest matching release is.  Inspired by crates.nvim.
//!
//! All hints are computed on demand; scanning is limited to the
//! client-supplied visible range.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::{
    InlayHint, InlayHintKind, InlayHintLabel, InlayHintParams, InlayHintTooltip, MarkupContent,
    MarkupKind, Range, Url,
};
use ropey::Rope;
use std::collections::HashMap;
use tfls_parser::{ReferenceKind, hcl_span_to_lsp_range};
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub async fn inlay_hint(
    backend: &Backend,
    params: InlayHintParams,
) -> jsonrpc::Result<Option<Vec<InlayHint>>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let Some(body) = doc.parsed.body.as_ref() else {
        return Ok(None);
    };

    let mut hints = Vec::new();

    // --- Variable defaults ----------------------------------------------
    let defaults = collect_variable_defaults(body);
    if !defaults.is_empty() {
        for reference in &doc.references {
            if let ReferenceKind::Variable { name } = &reference.kind {
                let range = reference.location.range();
                if !within(&params.range, range) {
                    continue;
                }
                if let Some(def) = defaults.get(name) {
                    hints.push(InlayHint {
                        position: range.end,
                        label: InlayHintLabel::String(format!(" = {def}")),
                        kind: Some(InlayHintKind::PARAMETER),
                        tooltip: None,
                        text_edits: None,
                        padding_left: Some(true),
                        padding_right: None,
                        data: None,
                    });
                }
            }
        }
    }

    // --- Version freshness ----------------------------------------------
    let config = backend.state.config.snapshot();
    hints.extend(version_hints(
        body,
        &doc.rope,
        &params.range,
        config.stale_version_days,
    ));

    // --- OpenTofu-only portability hints --------------------------------
    hints.extend(lifecycle_enabled_hints(
        body,
        &doc.rope,
        &params.range,
        &uri,
    ));

    if hints.is_empty() { Ok(None) } else { Ok(Some(hints)) }
}

// -------------------------------------------------------------------------
//  `lifecycle { enabled = … }` portability hints
//
//  `enabled` is OpenTofu 1.11+. On portable (`.tf` / `.tf.json`) files
//  we surface a quiet inline marker so the author knows the code
//  won't run under Terraform — no squiggle, no problems-panel entry.
//  Silent on `.tofu` / `.tofu.json`.
// -------------------------------------------------------------------------

fn is_opentofu_file(uri: &Url) -> bool {
    let path = uri.path();
    path.ends_with(".tofu") || path.ends_with(".tofu.json")
}

fn lifecycle_enabled_hints(
    body: &Body,
    rope: &Rope,
    visible: &Range,
    uri: &Url,
) -> Vec<InlayHint> {
    let mut out = Vec::new();
    if is_opentofu_file(uri) {
        return out;
    }
    for structure in body.iter() {
        let Some(block) = structure.as_block() else { continue };
        if !matches!(block.ident.as_str(), "resource" | "data") {
            continue;
        }
        for inner in block.body.iter() {
            let Some(lifecycle) = inner.as_block() else { continue };
            if lifecycle.ident.as_str() != "lifecycle" {
                continue;
            }
            for entry in lifecycle.body.iter() {
                let Some(attr) = entry.as_attribute() else { continue };
                if attr.key.as_str() != "enabled" {
                    continue;
                }
                let Some(span) = attr.value.span() else { continue };
                let Ok(range) = hcl_span_to_lsp_range(rope, span) else { continue };
                if !within(visible, range) {
                    continue;
                }
                out.push(InlayHint {
                    position: range.end,
                    label: InlayHintLabel::String(" OpenTofu 1.11+".to_string()),
                    kind: Some(InlayHintKind::PARAMETER),
                    tooltip: Some(InlayHintTooltip::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value:
                            "`enabled` is an [OpenTofu 1.11+ meta-argument](https://opentofu.org/docs/language/meta-arguments/enabled/).\n\nTerraform does not support it — if this module is OpenTofu-only, rename the file to `.tofu` (or `.tofu.json`) to suppress this hint. For Terraform-compatible code, use `count = var.create ? 1 : 0` or `for_each` instead."
                                .to_string(),
                    })),
                    text_edits: None,
                    padding_left: Some(true),
                    padding_right: None,
                    data: None,
                });
            }
        }
    }
    out
}

// -------------------------------------------------------------------------
//  Version freshness hints
// -------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum VersionSource {
    TerraformCli,
    Provider { namespace: String, name: String },
    Module {
        namespace: String,
        name: String,
        provider: String,
    },
}

fn version_hints(body: &Body, rope: &Rope, visible: &Range, stale_days: u32) -> Vec<InlayHint> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else { continue };
        match block.ident.as_str() {
            "terraform" => walk_terraform(&block.body, rope, visible, stale_days, &mut out),
            "module" => walk_module(&block.body, rope, visible, stale_days, &mut out),
            _ => {}
        }
    }
    out
}

fn walk_terraform(
    body: &Body,
    rope: &Rope,
    visible: &Range,
    stale_days: u32,
    out: &mut Vec<InlayHint>,
) {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if attr.key.as_str() == "required_version" {
                emit_for_attr(&attr.value, rope, visible, &VersionSource::TerraformCli, stale_days, out);
            }
        } else if let Some(nested) = structure.as_block() {
            if nested.ident.as_str() == "required_providers" {
                walk_required_providers(&nested.body, rope, visible, stale_days, out);
            }
        }
    }
}

fn walk_required_providers(
    body: &Body,
    rope: &Rope,
    visible: &Range,
    stale_days: u32,
    out: &mut Vec<InlayHint>,
) {
    for structure in body.iter() {
        let Some(attr) = structure.as_attribute() else { continue };
        let Expression::Object(obj) = &attr.value else { continue };
        let mut source_str: Option<String> = None;
        let mut version_expr: Option<&Expression> = None;
        for (key, value) in obj.iter() {
            let key_str = object_key_as_str(key);
            let Some(k) = key_str else { continue };
            match k.as_str() {
                "source" => source_str = literal_string(value.expr()),
                "version" => version_expr = Some(value.expr()),
                _ => {}
            }
        }
        let Some(source) = source_str.and_then(|s| parse_provider_source(&s)) else { continue };
        let Some(expr) = version_expr else { continue };
        emit_for_attr(
            expr,
            rope,
            visible,
            &VersionSource::Provider {
                namespace: source.0,
                name: source.1,
            },
            stale_days,
            out,
        );
    }
}

fn walk_module(
    body: &Body,
    rope: &Rope,
    visible: &Range,
    stale_days: u32,
    out: &mut Vec<InlayHint>,
) {
    let mut source_str: Option<String> = None;
    let mut version_expr: Option<&Expression> = None;
    for structure in body.iter() {
        let Some(attr) = structure.as_attribute() else { continue };
        match attr.key.as_str() {
            "source" => source_str = literal_string(&attr.value),
            "version" => version_expr = Some(&attr.value),
            _ => {}
        }
    }
    let Some(expr) = version_expr else { return };
    let Some((ns, name, provider)) = source_str.as_deref().and_then(parse_module_source) else {
        return;
    };
    emit_for_attr(
        expr,
        rope,
        visible,
        &VersionSource::Module {
            namespace: ns,
            name,
            provider,
        },
        stale_days,
        out,
    );
}

fn emit_for_attr(
    expr: &Expression,
    rope: &Rope,
    visible: &Range,
    source: &VersionSource,
    stale_days: u32,
    out: &mut Vec<InlayHint>,
) {
    let Some(span) = expr.span() else { return };
    let Ok(range) = hcl_span_to_lsp_range(rope, span) else { return };
    if !within(visible, range) {
        return;
    }
    let Some(raw) = literal_string(expr) else { return };
    let parsed = tfls_core::version_constraint::parse(&raw);
    if !parsed.errors.is_empty() || parsed.constraints.is_empty() {
        return;
    }
    let Some(entries) = read_versions_with_dates(source) else { return };
    let label = compose_label(&parsed.constraints, &entries, stale_days);
    let Some(label) = label else { return };
    out.push(InlayHint {
        position: range.end,
        label: InlayHintLabel::String(label),
        kind: Some(InlayHintKind::TYPE),
        tooltip: None,
        text_edits: None,
        padding_left: Some(true),
        padding_right: None,
        data: None,
    });
}

fn compose_label(
    constraints: &[tfls_core::version_constraint::Constraint],
    entries: &[(String, Option<String>)],
    stale_days: u32,
) -> Option<String> {
    use tfls_core::version_constraint::satisfies_all;

    // Latest overall = first entry (list arrives semver-desc sorted
    // upstream; see `merge_with_provenance`). Fallback: scan manually.
    let latest_overall = entries.first().map(|(v, _)| v.clone())?;
    // Latest matching = first entry that satisfies the constraint.
    let latest_matching = entries
        .iter()
        .find(|(v, _)| satisfies_all(constraints, v));

    match latest_matching {
        Some((v, date)) if *v == latest_overall => {
            // Exact latest — check staleness by date if we have it.
            if let Some(age) = age_days(date.as_deref()) {
                if stale_days > 0 && age > stale_days as i64 {
                    return Some(format!(
                        "✓ {v}   ⚠ {}",
                        humanise_age(age)
                    ));
                }
            }
            Some(format!("✓ {v}"))
        }
        Some((v, date)) => {
            // Newer available. Include staleness of the matched version.
            let mut label = format!("→ {latest_overall}  (pinned: {v})");
            if let Some(age) = age_days(date.as_deref()) {
                if stale_days > 0 && age > stale_days as i64 {
                    label.push_str(&format!("  ⚠ {}", humanise_age(age)));
                }
            }
            Some(label)
        }
        None => {
            // No match — diagnostic warning already flags this; skip
            // the hint to avoid a double signal.
            None
        }
    }
}

/// Number of whole days between `date` (ISO 8601 UTC, e.g.
/// `2025-06-12T18:33:49Z`) and now. Returns `None` for unparseable
/// input.
fn age_days(date: Option<&str>) -> Option<i64> {
    let s = date?;
    // Parse just the date prefix `YYYY-MM-DD`. Rough approximation
    // (ignores time-of-day) — plenty precise for month-scale staleness.
    let (year, rest) = s.split_once('-')?;
    let (month, rest) = rest.split_once('-')?;
    let day = rest.get(..2)?;
    let year: i32 = year.parse().ok()?;
    let month: u32 = month.parse().ok()?;
    let day: u32 = day.parse().ok()?;
    // Unix day at 00:00 UTC for the given date.
    let released_unix_day = days_from_civil(year, month, day)?;
    let now_unix_day = now_as_unix_day();
    Some(now_unix_day - released_unix_day)
}

/// Hinnant's "days_from_civil" — the well-known O(1) pure-integer
/// conversion from (Y, M, D) to a day number.
fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era as i64 * 146_097 + doe as i64 - 719_468)
}

fn now_as_unix_day() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    now / 86_400
}

fn humanise_age(days: i64) -> String {
    if days < 30 {
        format!("{days}d old")
    } else if days < 365 {
        format!("{}mo old", days / 30)
    } else {
        let years = days / 365;
        let months = (days % 365) / 30;
        if months == 0 {
            format!("{years}y old")
        } else {
            format!("{years}y {months}mo old")
        }
    }
}

// -------------------------------------------------------------------------
//  Cache readers (inlay-hint-specific — reads the disk caches populated
//  by the completion path; no network)
// -------------------------------------------------------------------------

fn read_versions_with_dates(
    source: &VersionSource,
) -> Option<Vec<(String, Option<String>)>> {
    match source {
        VersionSource::TerraformCli => read_tool_cache(),
        VersionSource::Provider { namespace, name } => {
            read_provider_cache(namespace, name)
        }
        VersionSource::Module {
            namespace,
            name,
            provider,
        } => read_module_cache(namespace, name, provider),
    }
}

fn read_tool_cache() -> Option<Vec<(String, Option<String>)>> {
    #[derive(serde::Deserialize)]
    struct Entry {
        version: String,
        #[serde(default)]
        published_at: Option<String>,
    }
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for slug in &["terraform", "opentofu"] {
        let Some(path) = cache_path(&["tool-versions", &format!("{slug}.json")]) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(&path) else { continue };
        // New richer shape is a list of objects; old shape was a list of
        // bare version strings — accept both.
        if let Ok(rich) = serde_json::from_str::<Vec<Entry>>(&body) {
            for e in rich {
                if seen.insert(e.version.clone()) {
                    out.push((e.version, e.published_at));
                }
            }
            continue;
        }
        if let Ok(plain) = serde_json::from_str::<Vec<String>>(&body) {
            for v in plain {
                if seen.insert(v.clone()) {
                    out.push((v, None));
                }
            }
        }
    }
    sort_versions_desc(&mut out);
    if out.is_empty() { None } else { Some(out) }
}

fn read_provider_cache(namespace: &str, name: &str) -> Option<Vec<(String, Option<String>)>> {
    // Two date caches: Terraform registry v2 (`dates/`) and OpenTofu
    // via GitHub fallback (`dates-opentofu/`). Terraform wins for any
    // version present in both; OpenTofu fills holes.
    let mut dates: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(path) = cache_path(&[
        "registry-versions",
        "dates",
        &sanitise(namespace),
        &sanitise(name),
        "dates.json",
    ]) {
        if let Ok(body) = std::fs::read_to_string(&path) {
            if let Ok(map) = serde_json::from_str::<std::collections::HashMap<String, String>>(&body) {
                dates.extend(map);
            }
        }
    }
    if let Some(path) = cache_path(&[
        "registry-versions",
        "dates-opentofu",
        &sanitise(namespace),
        &sanitise(name),
        "dates.json",
    ]) {
        if let Ok(body) = std::fs::read_to_string(&path) {
            if let Ok(map) = serde_json::from_str::<std::collections::HashMap<String, String>>(&body) {
                for (v, d) in map {
                    dates.entry(v).or_insert(d);
                }
            }
        }
    }
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for reg in &["terraform", "opentofu"] {
        let Some(path) = cache_path(&[
            "registry-versions",
            &sanitise(reg),
            &sanitise(namespace),
            &sanitise(name),
            "versions.json",
        ]) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(&path) else { continue };
        let Ok(vs) = serde_json::from_str::<Vec<String>>(&body) else { continue };
        for v in vs {
            if seen.insert(v.clone()) {
                let d = dates.get(&v).cloned();
                out.push((v, d));
            }
        }
    }
    sort_versions_desc(&mut out);
    if out.is_empty() { None } else { Some(out) }
}

fn read_module_cache(
    namespace: &str,
    name: &str,
    provider: &str,
) -> Option<Vec<(String, Option<String>)>> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for reg in &["terraform", "opentofu"] {
        let Some(path) = cache_path(&[
            "registry-versions",
            "modules",
            &sanitise(reg),
            &sanitise(namespace),
            &sanitise(name),
            &sanitise(provider),
            "versions.json",
        ]) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(&path) else { continue };
        let Ok(vs) = serde_json::from_str::<Vec<String>>(&body) else { continue };
        for v in vs {
            if seen.insert(v.clone()) {
                out.push((v, None));
            }
        }
    }
    sort_versions_desc(&mut out);
    if out.is_empty() { None } else { Some(out) }
}

fn sort_versions_desc(entries: &mut [(String, Option<String>)]) {
    entries.sort_by_key(|e| std::cmp::Reverse(semver_tuple(&e.0)));
}

/// Minimal semver key for inlay-hint sorting only. Not shared with
/// `tfls-provider-protocol`'s private copy because pulling it across
/// the crate boundary for ~20 LOC isn't worth the dependency churn.
fn semver_tuple(v: &str) -> (i64, i64, i64, i32, String) {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let core = core.split('+').next().unwrap_or(core);
    let mut parts = core.splitn(3, '.');
    let major: Option<i64> = parts.next().and_then(|s| s.parse().ok());
    let minor: Option<i64> = parts.next().and_then(|s| s.parse().ok());
    let patch: Option<i64> = parts.next().and_then(|s| s.parse().ok());
    match major {
        Some(ma) => (
            ma,
            minor.unwrap_or(0),
            patch.unwrap_or(0),
            if pre.is_some() { 0 } else { 1 },
            pre.unwrap_or("").to_string(),
        ),
        None => (i64::MIN, 0, 0, 0, v.to_string()),
    }
}

fn cache_path(segments: &[&str]) -> Option<std::path::PathBuf> {
    let mut root = if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        std::path::PathBuf::from(dir).join("terraform-ls-rs")
    } else if let Some(home) = std::env::var_os("HOME") {
        std::path::PathBuf::from(home)
            .join(".cache")
            .join("terraform-ls-rs")
    } else {
        return None;
    };
    for seg in segments {
        root = root.join(seg);
    }
    Some(root)
}

fn sanitise(component: &str) -> String {
    component
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn object_key_as_str(key: &hcl_edit::expr::ObjectKey) -> Option<String> {
    match key {
        hcl_edit::expr::ObjectKey::Ident(d) => Some(d.as_str().to_string()),
        hcl_edit::expr::ObjectKey::Expression(Expression::String(s)) => {
            Some(s.as_str().to_string())
        }
        _ => None,
    }
}

fn literal_string(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(s.as_str().to_string()),
        Expression::StringTemplate(t) => {
            let mut collected = String::new();
            for element in t.iter() {
                match element {
                    hcl_edit::template::Element::Literal(lit) => {
                        collected.push_str(lit.as_str())
                    }
                    _ => return None,
                }
            }
            Some(collected)
        }
        _ => None,
    }
}

fn parse_provider_source(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let mut parts = s.splitn(3, '/');
    let a = parts.next()?;
    let b = parts.next()?;
    if let Some(c) = parts.next() {
        Some((b.to_string(), c.to_string()))
    } else {
        Some((a.to_string(), b.to_string()))
    }
}

fn parse_module_source(s: &str) -> Option<(String, String, String)> {
    let s = s.trim();
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

fn collect_variable_defaults(body: &Body) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "variable" {
            continue;
        }
        let Some(name) = first_label(block) else {
            continue;
        };
        for s in block.body.iter() {
            if let Some(attr) = s.as_attribute() {
                if attr.key.as_str() == "default" {
                    if let Some(lit) = literal_scalar(&attr.value) {
                        out.insert(name.to_string(), lit);
                    }
                    break;
                }
            }
        }
    }
    out
}

fn first_label(block: &Block) -> Option<&str> {
    block.labels.first().map(|l| match l {
        BlockLabel::String(s) => s.value().as_str(),
        BlockLabel::Ident(i) => i.as_str(),
    })
}

/// Return the source-level representation of a literal scalar
/// expression (string, number, bool). Compound expressions are
/// skipped so hints don't get noisy.
fn literal_scalar(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(format!("\"{}\"", s.value())),
        Expression::Number(n) => Some(n.value().to_string()),
        Expression::Bool(b) => Some(b.value().to_string()),
        Expression::Null(_) => Some("null".to_string()),
        _ => None,
    }
}

fn within(outer: &Range, inner: Range) -> bool {
    (inner.start.line, inner.start.character)
        >= (outer.start.line, outer.start.character)
        && (inner.end.line, inner.end.character) <= (outer.end.line, outer.end.character)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    #[test]
    fn collects_string_default() {
        let body = parse_source(r#"variable "region" { default = "us-east-1" }"#)
            .body
            .expect("parses");
        let defs = collect_variable_defaults(&body);
        assert_eq!(defs.get("region"), Some(&"\"us-east-1\"".to_string()));
    }

    #[test]
    fn collects_numeric_default() {
        let body = parse_source(r#"variable "count" { default = 3 }"#)
            .body
            .expect("parses");
        let defs = collect_variable_defaults(&body);
        assert_eq!(defs.get("count"), Some(&"3".to_string()));
    }

    #[test]
    fn skips_non_literal_default() {
        let body = parse_source(r#"variable "x" { default = [1, 2, 3] }"#)
            .body
            .expect("parses");
        let defs = collect_variable_defaults(&body);
        assert!(!defs.contains_key("x"));
    }

    #[test]
    fn skips_variables_without_default() {
        let body = parse_source(r#"variable "x" {}"#).body.expect("parses");
        let defs = collect_variable_defaults(&body);
        assert!(defs.is_empty());
    }

    // --- lifecycle.enabled portability hint --------------------------

    fn wide_range() -> Range {
        Range {
            start: lsp_types::Position::new(0, 0),
            end: lsp_types::Position::new(9999, 9999),
        }
    }

    fn make_hints(src: &str, uri_str: &str) -> Vec<InlayHint> {
        let body = parse_source(src).body.expect("parses");
        let rope = Rope::from_str(src);
        let uri = Url::parse(uri_str).expect("url");
        lifecycle_enabled_hints(&body, &rope, &wide_range(), &uri)
    }

    #[test]
    fn enabled_in_tf_file_emits_hint() {
        let hints = make_hints(
            r#"resource "aws_instance" "x" {
              ami = "ami-1"
              lifecycle {
                enabled = true
              }
            }"#,
            "file:///m/main.tf",
        );
        assert_eq!(hints.len(), 1, "got: {hints:?}");
        let h = &hints[0];
        match &h.label {
            InlayHintLabel::String(s) => {
                assert!(
                    s.contains("OpenTofu"),
                    "label should mention OpenTofu; got {s:?}"
                );
            }
            other => panic!("expected string label, got {other:?}"),
        }
        match &h.tooltip {
            Some(InlayHintTooltip::MarkupContent(mc)) => {
                assert!(mc.value.contains("OpenTofu"), "tooltip missing 'OpenTofu'");
                assert!(mc.value.contains("count"), "tooltip missing 'count' fallback");
            }
            other => panic!("expected markdown tooltip, got {other:?}"),
        }
    }

    #[test]
    fn enabled_in_tf_json_file_emits_hint() {
        let hints = make_hints(
            r#"resource "aws_instance" "x" {
              lifecycle {
                enabled = true
              }
            }"#,
            "file:///m/main.tf.json",
        );
        assert_eq!(hints.len(), 1, "got: {hints:?}");
    }

    #[test]
    fn enabled_in_tofu_file_emits_no_hint() {
        let hints = make_hints(
            r#"resource "aws_instance" "x" {
              lifecycle {
                enabled = true
              }
            }"#,
            "file:///m/main.tofu",
        );
        assert!(hints.is_empty(), "got: {hints:?}");
    }

    #[test]
    fn enabled_in_tofu_json_file_emits_no_hint() {
        let hints = make_hints(
            r#"resource "aws_instance" "x" {
              lifecycle {
                enabled = true
              }
            }"#,
            "file:///m/main.tofu.json",
        );
        assert!(hints.is_empty(), "got: {hints:?}");
    }

    #[test]
    fn enabled_in_data_lifecycle_is_also_hinted() {
        let hints = make_hints(
            r#"data "aws_ami" "x" {
              lifecycle {
                enabled = true
              }
            }"#,
            "file:///m/main.tf",
        );
        assert_eq!(hints.len(), 1, "got: {hints:?}");
    }

    #[test]
    fn no_hint_when_enabled_is_absent() {
        let hints = make_hints(
            r#"resource "aws_instance" "x" {
              lifecycle {
                create_before_destroy = true
              }
            }"#,
            "file:///m/main.tf",
        );
        assert!(hints.is_empty(), "got: {hints:?}");
    }
}
