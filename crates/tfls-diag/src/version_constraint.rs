//! Real-time validation for Terraform / OpenTofu version constraints.
//!
//! Walks the three attributes where the string value is a constraint:
//! - `required_version` inside the top-level `terraform { }` block
//! - `version` inside each `required_providers { NAME = { … } }` entry
//! - `version` directly inside `module "…" { … }` blocks
//!
//! Emits two diagnostic flavours:
//! - **Error** for syntax problems (unknown operator, missing version,
//!   malformed version, trailing comma) — surfaced via
//!   `tfls_core::version_constraint::parse`.
//! - **Warning** for semantic problems (no published version of the
//!   target matches the constraint). The no-match check is best-effort
//!   and only fires when the caller's `VersionCacheLookup` already has
//!   cached versions; it never issues network requests, so diagnostics
//!   stay synchronous.

use hcl_edit::repr::Span;
use hcl_edit::structure::Body;
use lsp_types::{Diagnostic, DiagnosticSeverity};
use ropey::Rope;
use tfls_core::version_constraint::{Constraint, satisfies_all};
use tfls_parser::hcl_span_to_lsp_range;

/// Source the caller knows version lists for. Attributes are validated
/// against whichever source the dispatcher determined is relevant.
#[derive(Debug, Clone)]
pub enum ConstraintSource {
    /// Validate against `hashicorp/terraform` + `opentofu/opentofu`
    /// CLI releases.
    TerraformCli,
    /// Validate against `<namespace>/<name>` provider versions.
    Provider { namespace: String, name: String },
    /// Validate against `<namespace>/<name>/<provider>` module versions.
    Module {
        namespace: String,
        name: String,
        provider: String,
    },
}

/// Cache lookup passed in by the LSP backend — reads the on-disk
/// version lists populated by the completion path. Returning `None`
/// means "no data cached yet, skip the semantic check".
pub trait VersionCacheLookup {
    fn cached_versions(&self, source: &ConstraintSource) -> Option<Vec<String>>;
}

/// Walks `body` and returns diagnostics for every constraint attribute.
pub fn constraint_diagnostics(
    body: &Body,
    rope: &Rope,
    cache: &dyn VersionCacheLookup,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else { continue };
        match block.ident.as_str() {
            "terraform" => walk_terraform(&block.body, rope, cache, &mut out),
            "module" => walk_module(&block.body, rope, cache, &mut out),
            _ => {}
        }
    }
    out
}

fn walk_terraform(
    body: &Body,
    rope: &Rope,
    cache: &dyn VersionCacheLookup,
    out: &mut Vec<Diagnostic>,
) {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if attr.key.as_str() == "required_version" {
                validate_string_value_attr(
                    attr,
                    rope,
                    &ConstraintSource::TerraformCli,
                    cache,
                    out,
                );
            }
        } else if let Some(block) = structure.as_block() {
            if block.ident.as_str() == "required_providers" {
                walk_required_providers(&block.body, rope, cache, out);
            }
        }
    }
}

fn walk_required_providers(
    body: &Body,
    rope: &Rope,
    cache: &dyn VersionCacheLookup,
    out: &mut Vec<Diagnostic>,
) {
    // Each `NAME = { source = "…", version = "…" }` is an attribute
    // with an object expression as its value. Walk the object body for
    // `version` + `source` string attributes.
    for structure in body.iter() {
        let Some(attr) = structure.as_attribute() else { continue };
        let expr = &attr.value;
        let Some(obj) = object_body(expr) else { continue };
        let mut source_value: Option<String> = None;
        let mut version_value: Option<(String, std::ops::Range<usize>)> = None;
        for (key, value) in obj.iter() {
            let Some(key_str) = object_key_as_str(key) else { continue };
            if key_str == "source" {
                source_value = string_expression(value.expr());
            } else if key_str == "version" {
                if let (Some(s), Some(span)) =
                    (string_expression(value.expr()), value.expr().span())
                {
                    version_value = Some((s, span));
                }
            }
        }
        let Some(source) = source_value.and_then(|s| parse_provider_source(&s)) else {
            continue;
        };
        let constraint_source = ConstraintSource::Provider {
            namespace: source.0,
            name: source.1,
        };
        if let Some((version_str, version_span)) = version_value {
            validate_constraint_string(
                &version_str,
                version_span,
                rope,
                &constraint_source,
                cache,
                out,
            );
        }
    }
}

fn object_key_as_str(key: &hcl_edit::expr::ObjectKey) -> Option<String> {
    match key {
        hcl_edit::expr::ObjectKey::Ident(d) => Some(d.as_str().to_string()),
        hcl_edit::expr::ObjectKey::Expression(hcl_edit::expr::Expression::String(s)) => {
            Some(s.as_str().to_string())
        }
        _ => None,
    }
}

fn walk_module(
    body: &Body,
    rope: &Rope,
    cache: &dyn VersionCacheLookup,
    out: &mut Vec<Diagnostic>,
) {
    let mut source_str: Option<String> = None;
    let mut version_attr: Option<(String, std::ops::Range<usize>)> = None;
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            let key = attr.key.as_str();
            if key == "source" {
                if let Some(s) = string_expression(&attr.value) {
                    source_str = Some(s);
                }
            } else if key == "version" {
                if let (Some(s), Some(span)) =
                    (string_expression(&attr.value), attr.value.span())
                {
                    version_attr = Some((s, span));
                }
            }
        }
    }
    let Some((_version_str, span)) = version_attr else { return };
    // Syntax check runs regardless of source. Semantic runs only when
    // the source looks like a registry module path.
    let module_source = source_str
        .as_deref()
        .and_then(parse_module_source_parts)
        .map(|(ns, name, provider)| ConstraintSource::Module {
            namespace: ns,
            name,
            provider,
        });
    match module_source {
        Some(source) => {
            validate_constraint_string(&_version_str, span, rope, &source, cache, out);
        }
        None => {
            validate_constraint_string_syntax_only(span, rope, out);
        }
    }
}

/// Same as `validate_constraint_string` but skips the semantic
/// (no-match) stage — used for module versions when we can't
/// identify a registry source to compare against.
fn validate_constraint_string_syntax_only(
    value_span: std::ops::Range<usize>,
    rope: &Rope,
    out: &mut Vec<Diagnostic>,
) {
    if value_span.end <= value_span.start + 2 {
        return;
    }
    let inner_start = value_span.start + 1;
    let inner_end = value_span.end - 1;
    let inner = read_slice(rope, inner_start..inner_end);
    let parsed = tfls_core::version_constraint::parse(&inner);
    for err in &parsed.errors {
        let abs = (inner_start + err.span.start)..(inner_start + err.span.end);
        if let Ok(range) = hcl_span_to_lsp_range(rope, abs) {
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("terraform-ls-rs".to_string()),
                message: err.message.clone(),
                ..Default::default()
            });
        }
    }
}

fn validate_string_value_attr(
    attr: &hcl_edit::structure::Attribute,
    rope: &Rope,
    source: &ConstraintSource,
    cache: &dyn VersionCacheLookup,
    out: &mut Vec<Diagnostic>,
) {
    let Some(s) = string_expression(&attr.value) else { return };
    let Some(span) = attr.value.span() else { return };
    validate_constraint_string(&s, span, rope, source, cache, out);
}

fn validate_constraint_string(
    _raw_with_quotes: &str,
    value_span: std::ops::Range<usize>,
    rope: &Rope,
    source: &ConstraintSource,
    cache: &dyn VersionCacheLookup,
    out: &mut Vec<Diagnostic>,
) {
    // `value_span` covers the whole `"…"` including the quote
    // characters; the inner content starts at +1 and ends at -1.
    if value_span.end <= value_span.start + 2 {
        return;
    }
    let inner_start = value_span.start + 1;
    let inner_end = value_span.end - 1;
    let inner = read_slice(rope, inner_start..inner_end);
    let parsed = tfls_core::version_constraint::parse(&inner);

    for err in &parsed.errors {
        let abs = (inner_start + err.span.start)..(inner_start + err.span.end);
        if let Ok(range) = hcl_span_to_lsp_range(rope, abs) {
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("terraform-ls-rs".to_string()),
                message: err.message.clone(),
                ..Default::default()
            });
        }
    }

    if !parsed.errors.is_empty() || parsed.constraints.is_empty() {
        return;
    }
    let Some(versions) = cache.cached_versions(source) else { return };
    let any_match = versions
        .iter()
        .any(|v| satisfies_all(&parsed.constraints, v));
    if !any_match {
        if let Ok(range) = hcl_span_to_lsp_range(rope, inner_start..inner_end) {
            out.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("terraform-ls-rs".to_string()),
                message: format!(
                    "no published version of {} matches `{}`",
                    source_label(source),
                    inner
                ),
                ..Default::default()
            });
        }
    }
    // Keep Constraint in the import list — used by `satisfies_all`'s
    // signature. An explicit reference prevents dead-code churn later.
    let _: fn(&[Constraint], &str) -> bool = satisfies_all;
}

fn source_label(source: &ConstraintSource) -> String {
    match source {
        ConstraintSource::TerraformCli => "the Terraform / OpenTofu CLI".to_string(),
        ConstraintSource::Provider { namespace, name } => format!("{namespace}/{name}"),
        ConstraintSource::Module {
            namespace,
            name,
            provider,
        } => format!("{namespace}/{name}/{provider}"),
    }
}

// -- hcl-edit helpers ------------------------------------------------------

fn object_body(
    expr: &hcl_edit::expr::Expression,
) -> Option<&hcl_edit::expr::Object> {
    match expr {
        hcl_edit::expr::Expression::Object(o) => Some(o),
        _ => None,
    }
}

fn string_expression(expr: &hcl_edit::expr::Expression) -> Option<String> {
    match expr {
        hcl_edit::expr::Expression::String(s) => Some(s.as_str().to_string()),
        hcl_edit::expr::Expression::StringTemplate(t) => {
            // Treat interpolation-free string templates as their
            // literal content. Anything with interpolation is skipped
            // (we can't meaningfully parse `"~> ${var.v}"` statically).
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

fn read_slice(rope: &Rope, range: std::ops::Range<usize>) -> String {
    if range.end > rope.len_bytes() || range.start > range.end {
        return String::new();
    }
    rope.byte_slice(range.start..range.end).to_string()
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

fn parse_module_source_parts(s: &str) -> Option<(String, String, String)> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use hcl_edit::parser;

    struct EmptyCache;
    impl VersionCacheLookup for EmptyCache {
        fn cached_versions(&self, _: &ConstraintSource) -> Option<Vec<String>> {
            None
        }
    }

    struct StaticCache(Vec<String>);
    impl VersionCacheLookup for StaticCache {
        fn cached_versions(&self, _: &ConstraintSource) -> Option<Vec<String>> {
            Some(self.0.clone())
        }
    }

    fn parse_body(src: &str) -> (Body, Rope) {
        let body = parser::parse_body(src).expect("parse");
        (body, Rope::from_str(src))
    }

    #[test]
    fn rejects_malformed_required_version() {
        let src = "terraform {\n  required_version = \">== 1.0\"\n}\n";
        let (body, rope) = parse_body(src);
        let diags = constraint_diagnostics(&body, &rope, &EmptyCache);
        assert!(!diags.is_empty(), "expected diagnostic; got {diags:?}");
        assert!(diags.iter().any(|d| d.severity == Some(DiagnosticSeverity::ERROR)));
    }

    #[test]
    fn rejects_malformed_provider_version() {
        let src = "terraform {\n  required_providers {\n    aws = {\n      source = \"hashicorp/aws\"\n      version = \"1.x\"\n    }\n  }\n}\n";
        let (body, rope) = parse_body(src);
        let diags = constraint_diagnostics(&body, &rope, &EmptyCache);
        assert!(
            diags.iter().any(|d| d.message.contains("malformed")),
            "got {diags:?}"
        );
    }

    #[test]
    fn accepts_multi_constraint() {
        let src = "terraform {\n  required_version = \">= 1.0, < 2.0\"\n}\n";
        let (body, rope) = parse_body(src);
        let diags = constraint_diagnostics(&body, &rope, &EmptyCache);
        assert!(diags.is_empty(), "should be clean; got {diags:?}");
    }

    #[test]
    fn no_match_warning_when_cache_has_versions() {
        let src = "terraform {\n  required_providers {\n    aws = {\n      source = \"hashicorp/aws\"\n      version = \"< 0.0.1\"\n    }\n  }\n}\n";
        let (body, rope) = parse_body(src);
        let cache = StaticCache(vec!["1.0.0".to_string(), "5.99.0".to_string()]);
        let diags = constraint_diagnostics(&body, &rope, &cache);
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::WARNING)
                    && d.message.contains("no published version")),
            "got {diags:?}"
        );
    }

    #[test]
    fn empty_cache_suppresses_no_match_warning() {
        let src = "terraform {\n  required_providers {\n    aws = {\n      source = \"hashicorp/aws\"\n      version = \"< 0.0.1\"\n    }\n  }\n}\n";
        let (body, rope) = parse_body(src);
        let diags = constraint_diagnostics(&body, &rope, &EmptyCache);
        assert!(
            diags
                .iter()
                .all(|d| d.severity != Some(DiagnosticSeverity::WARNING)),
            "must not warn without cached versions; got {diags:?}"
        );
    }

    #[test]
    fn validates_module_version() {
        let src = "module \"x\" {\n  source = \"terraform-aws-modules/vpc/aws\"\n  version = \">== 5.0\"\n}\n";
        let (body, rope) = parse_body(src);
        let diags = constraint_diagnostics(&body, &rope, &EmptyCache);
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "got {diags:?}"
        );
    }
}
