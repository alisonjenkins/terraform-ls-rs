//! Generic scaffolding for "this Terraform construct is
//! superseded — flag it" diagnostics.
//!
//! Three live consumers (`null_resource`, `template_file`,
//! `template_dir`) all share the same shape: walk top-level
//! `<block_kind> "<label>" "X" { ... }` blocks, emit a WARNING
//! on the label range, suppress when the module's
//! `required_version` excludes the floor at which the
//! replacement landed. The deprecation modules now thin-wrap
//! `DeprecationRule` instead of repeating the walk + version
//! gate three times.
//!
//! Each rule is a plain `const` carrying the small set of
//! parameters that differ between deprecations. Adding a new
//! deprecation that fits this shape is a config entry +
//! one-line wrapper, not a fresh body walker.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::{BlockLabel, Body};
use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;

/// `(block_kind, label)` pairs for every hardcoded
/// deprecation rule across the crate. Used by
/// `schema_validation` to suppress duplicate schema-driven
/// warnings on resources / data sources we already emit a
/// richer message for. Keep in sync with the `pub mod
/// deprecated_*` modules in `lib.rs`.
pub const HARDCODED_DEPRECATION_LABELS: &[(&str, &str)] = &[
    ("resource", "null_resource"),
    ("data", "template_file"),
    ("data", "template_dir"),
    ("data", "null_data_source"),
    // AWS rename family — see `deprecated_aws_renames.rs`.
    // Each module has an `every_*_is_hardcoded_listed` test
    // that fails when this list drifts from the rule table.
    ("resource", "aws_alb"),
    ("resource", "aws_alb_listener"),
    ("resource", "aws_alb_listener_rule"),
    ("resource", "aws_alb_target_group"),
    ("resource", "aws_alb_target_group_attachment"),
    ("resource", "aws_s3_bucket_object"),
    ("data", "aws_s3_bucket_object"),
    ("data", "aws_s3_bucket_objects"),
    ("resource", "aws_kinesis_analytics_application"),
    // Kubernetes `_v1` rename family —
    // `deprecated_kubernetes_renames.rs`.
    ("resource", "kubernetes_pod"),
    ("resource", "kubernetes_deployment"),
    ("resource", "kubernetes_service"),
    ("resource", "kubernetes_namespace"),
    ("resource", "kubernetes_config_map"),
    ("resource", "kubernetes_secret"),
    ("resource", "kubernetes_role"),
    ("resource", "kubernetes_role_binding"),
    ("resource", "kubernetes_cluster_role"),
    ("resource", "kubernetes_cluster_role_binding"),
    ("resource", "kubernetes_persistent_volume"),
    ("resource", "kubernetes_persistent_volume_claim"),
    ("resource", "kubernetes_service_account"),
    ("resource", "kubernetes_stateful_set"),
    ("resource", "kubernetes_daemonset"),
    ("resource", "kubernetes_job"),
    ("resource", "kubernetes_cron_job"),
    ("resource", "kubernetes_network_policy"),
    ("resource", "kubernetes_ingress"),
    ("resource", "kubernetes_horizontal_pod_autoscaler"),
    // Azure (azurerm) split deprecations —
    // `deprecated_azurerm_blocks.rs`.
    ("resource", "azurerm_virtual_machine"),
    ("resource", "azurerm_virtual_machine_scale_set"),
    // GCP (google) block deprecations —
    // `deprecated_google_blocks.rs`.
    ("resource", "google_dataflow_job"),
    // Vault block deprecations —
    // `deprecated_vault_blocks.rs`.
    ("resource", "vault_generic_secret"),
];

/// True when `(block_kind, label)` is covered by a hardcoded
/// rule — caller should suppress its own diagnostic on this
/// block (the hardcoded rule produces a richer message and
/// often a paired code action).
pub fn is_hardcoded_deprecation(block_kind: &str, label: &str) -> bool {
    HARDCODED_DEPRECATION_LABELS
        .iter()
        .any(|(k, l)| *k == block_kind && *l == label)
}

/// Static description of one "X is deprecated, prefer Y" rule.
#[derive(Debug, Clone, Copy)]
pub struct DeprecationRule {
    /// HCL block kind to match — typically `"resource"` or
    /// `"data"`.
    pub block_kind: &'static str,
    /// First block label whose presence triggers the
    /// diagnostic (e.g. `"null_resource"`, `"template_file"`).
    pub label: &'static str,
    /// Where the version constraint that gates this rule
    /// lives. Terraform-core deprecations use
    /// [`Gate::TerraformVersion`]; provider-specific ones use
    /// [`Gate::ProviderVersion`].
    pub gate: Gate,
    /// User-visible message — should name the replacement and
    /// (when applicable) gesture at the matching code action.
    pub message: &'static str,
}

/// Where the rule's version constraint comes from.
#[derive(Debug, Clone, Copy)]
pub enum Gate {
    /// `terraform { required_version = "..." }`. The threshold
    /// is the lowest Terraform-core version that ships the
    /// replacement (e.g. `"1.4.0"` for `terraform_data`).
    /// Constraints whose minimum admitted version is below the
    /// threshold suppress the rule.
    TerraformVersion { threshold: &'static str },
    /// `terraform { required_providers { <name> = ... } }` —
    /// either short form `"~> 4.0"` or long form
    /// `{ source = "...", version = "~> 4.0" }`. Used for
    /// provider-specific deprecations (e.g. AWS provider 4.0+
    /// deprecated `aws_alb` in favour of `aws_lb`). The
    /// threshold is the version at which the replacement is
    /// available, NOT the version that deprecated the original
    /// — same suppression semantics apply.
    ProviderVersion {
        provider: &'static str,
        threshold: &'static str,
    },
}

impl DeprecationRule {
    /// Convenience accessor to the rule's threshold version,
    /// regardless of which gate kind the rule uses.
    pub fn threshold(&self) -> &'static str {
        match &self.gate {
            Gate::TerraformVersion { threshold } => threshold,
            Gate::ProviderVersion { threshold, .. } => threshold,
        }
    }
}

/// Body-only diagnostic emit. Computes the per-body version
/// gate from the body's own `required_version` (if any). Multi-
/// file modules should prefer [`diagnostics_for_module`] —
/// `required_version` typically lives in `versions.tf`, not the
/// file being scanned.
pub fn diagnostics(rule: &DeprecationRule, body: &Body, rope: &Rope) -> Vec<Diagnostic> {
    diagnostics_for_module(rule, body, rope, body_supports(rule, body))
}

/// Module-aware emit. Caller supplies the precomputed
/// `supports` flag (true = replacement available, fire) so a
/// constraint declared in a sibling can correctly gate this
/// body.
pub fn diagnostics_for_module(
    rule: &DeprecationRule,
    body: &Body,
    rope: &Rope,
    supports: bool,
) -> Vec<Diagnostic> {
    if !supports {
        return Vec::new();
    }
    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != rule.block_kind {
            continue;
        }
        let Some(label) = block.labels.first() else {
            continue;
        };
        if label_str(label) != Some(rule.label) {
            continue;
        }
        let Some(span) = label.span() else { continue };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: rule.message.to_string(),
            tags: Some(vec![DiagnosticTag::DEPRECATED]),
            ..Default::default()
        });
    }
    out
}

/// Multi-rule body walk. Visits every top-level
/// `resource` / `data` block once and emits a diagnostic for
/// every rule in `rules` whose `(block_kind, label)` matches
/// AND whose gate currently admits the replacement (per
/// `rule_supported`).
///
/// One walk regardless of rule count — so a provider with
/// dozens of renames pays O(blocks) total, not
/// O(blocks × rules). The `rule_supported` callback lets the
/// LSP layer pre-cache module-aggregated constraint decisions
/// (each rule's gate evaluation is otherwise cheap but the
/// `required_providers` extraction would re-walk siblings per
/// rule without caching upstream).
pub fn diagnostics_from_table(
    body: &Body,
    rope: &Rope,
    rules: &[DeprecationRule],
    rule_supported: &dyn Fn(&DeprecationRule) -> bool,
) -> Vec<Diagnostic> {
    use std::collections::HashMap;
    // Index by (block_kind, label) so each block only does one
    // lookup. `&str` keys borrow from the static rule strings —
    // zero allocation per call.
    let mut by_key: HashMap<(&str, &str), &DeprecationRule> = HashMap::new();
    for rule in rules {
        if !rule_supported(rule) {
            continue;
        }
        by_key.insert((rule.block_kind, rule.label), rule);
    }
    if by_key.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        let kind = block.ident.as_str();
        let Some(label) = block.labels.first() else {
            continue;
        };
        let label_text = label_str(label).unwrap_or("");
        let Some(rule) = by_key.get(&(kind, label_text)) else {
            continue;
        };
        let Some(span) = label.span() else { continue };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        out.push(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("terraform-ls-rs".to_string()),
            message: rule.message.to_string(),
            tags: Some(vec![DiagnosticTag::DEPRECATED]),
            ..Default::default()
        });
    }
    out
}

/// Body-only support test: pulls the rule's relevant
/// constraint string from `body` (per `gate` kind), returns
/// `true` when the rule's threshold is admitted. Used by
/// per-provider table modules' convenience entry points.
/// Multi-file modules should prefer the LSP layer's
/// module-aggregated path since constraints typically live in
/// `versions.tf`, not the file the user is editing.
pub fn body_supports_rule(rule: &DeprecationRule, body: &Body) -> bool {
    let constraint = match &rule.gate {
        Gate::TerraformVersion { .. } => extract_required_version(body),
        Gate::ProviderVersion { provider, .. } => extract_required_provider_version(body, provider),
    };
    let Some(c) = constraint else { return true };
    supports(rule, &c)
}

/// True when `constraint` admits a version at or above the
/// rule's threshold floor (i.e. the replacement exists in
/// every version the user might run). Empty / loose
/// constraints fall through to `true` — we can't suppress on
/// absence of evidence.
pub fn supports(rule: &DeprecationRule, constraint: &str) -> bool {
    let parsed = tfls_core::version_constraint::parse(constraint);
    if parsed.constraints.is_empty() {
        return true;
    }
    let Some(min) = tfls_core::version_constraint::min_admitted_version(&parsed.constraints) else {
        return false;
    };
    tfls_core::version_constraint::version_at_least(min, rule.threshold())
}

/// Lock-aware variant of [`supports`]. When `locked` is `Some`
/// and the rule is a [`Gate::ProviderVersion`], the locked
/// version is the source of truth — it's what `terraform
/// plan/apply` runs, not the lower bound of the declared
/// constraint. Falls back to constraint-based gating when no
/// locked version is available (lock file missing or provider
/// not listed).
///
/// Behaviour:
/// - `locked = Some(v)` and rule is `ProviderVersion`: compare
///   `v >= rule.threshold` directly. (Constraint is ignored —
///   if the lock contradicts the declared constraint that's a
///   stale-lock issue for the user to reconcile, but the lock
///   is still what's installed.)
/// - `locked = Some(_)` but rule is `TerraformVersion`: lock
///   doesn't track Terraform CLI versions, fall through.
/// - `locked = None`: fall through to [`supports`] with the
///   constraint, or `true` if no constraint either.
pub fn supports_with_lock(
    rule: &DeprecationRule,
    constraint: Option<&str>,
    locked: Option<&semver::Version>,
) -> bool {
    if let (Gate::ProviderVersion { .. }, Some(v)) = (&rule.gate, locked) {
        let threshold = match semver::Version::parse(rule.threshold()) {
            Ok(t) => t,
            Err(_) => {
                // Threshold can be loose like "4.0" — try padding
                // to "4.0.0" before falling through.
                match semver::Version::parse(&format!("{}.0", rule.threshold())) {
                    Ok(t) => t,
                    Err(_) => match semver::Version::parse(&format!("{}.0.0", rule.threshold())) {
                        Ok(t) => t,
                        Err(_) => return supports_via_constraint(rule, constraint),
                    },
                }
            }
        };
        return *v >= threshold;
    }
    supports_via_constraint(rule, constraint)
}

fn supports_via_constraint(rule: &DeprecationRule, constraint: Option<&str>) -> bool {
    match constraint {
        Some(c) => supports(rule, c),
        None => true,
    }
}

/// Extract the literal `required_version = "..."` string from
/// the top-level `terraform { }` block, if present. Empty
/// fragments / non-string forms are ignored.
pub fn extract_required_version(body: &Body) -> Option<String> {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if block.ident.as_str() != "terraform" {
            continue;
        }
        for sub in block.body.iter() {
            let Some(attr) = sub.as_attribute() else {
                continue;
            };
            if attr.key.as_str() != "required_version" {
                continue;
            }
            return read_string_value(&attr.value);
        }
    }
    None
}

fn body_supports(rule: &DeprecationRule, body: &Body) -> bool {
    let constraint = match &rule.gate {
        Gate::TerraformVersion { .. } => extract_required_version(body),
        Gate::ProviderVersion { provider, .. } => extract_required_provider_version(body, provider),
    };
    let Some(c) = constraint else { return true };
    supports(rule, &c)
}

/// Extract the version constraint declared for `provider_name`
/// in any `terraform { required_providers { ... } }` block in
/// `body`. Recognises both forms:
///
/// ```hcl
/// terraform {
///   required_providers {
///     aws = "~> 4.0"                 # short form
///     // OR
///     aws = { source = "...", version = "~> 4.0" }  # long form
///   }
/// }
/// ```
///
/// Returns `None` when the provider isn't listed, when the
/// long form omits `version`, or when the constraint is a
/// non-string expression.
pub fn extract_required_provider_version(body: &Body, provider_name: &str) -> Option<String> {
    let canonical = provider_ns_name(provider_name);
    for structure in body.iter() {
        let Some(tf_block) = structure.as_block() else {
            continue;
        };
        if tf_block.ident.as_str() != "terraform" {
            continue;
        }
        for inner in tf_block.body.iter() {
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
                let local_key = attr.key.as_str();
                // Short form: `<key> = "constraint"`. Source is implicit
                // `hashicorp/<key>`; match canonically so an aliased key
                // (e.g. `awscloud = "~> 4.0"`) still resolves, and a fork
                // doesn't.
                if let Some(s) = read_string_value(&attr.value) {
                    if entry_matches_provider(local_key, None, &canonical) {
                        return Some(s);
                    }
                    continue;
                }
                if let Expression::Object(obj) = &attr.value {
                    let mut version = None;
                    let mut source = None;
                    for (k, v) in obj.iter() {
                        match object_key_name(k) {
                            Some("version") => version = read_string_value(v.expr()),
                            Some("source") => source = read_string_value(v.expr()),
                            _ => {}
                        }
                    }
                    if entry_matches_provider(local_key, source.as_deref(), &canonical) {
                        if let Some(v) = version {
                            return Some(v);
                        }
                    }
                }
            }
        }
    }
    None
}

/// The `namespace/name` of an `ObjectKey`, lowercased, when it's a plain
/// identifier / quoted string.
fn object_key_name(k: &hcl_edit::expr::ObjectKey) -> Option<&str> {
    match k {
        hcl_edit::expr::ObjectKey::Ident(id) => Some(id.as_str()),
        hcl_edit::expr::ObjectKey::Expression(Expression::Variable(var)) => {
            Some(var.value().as_str())
        }
        hcl_edit::expr::ObjectKey::Expression(Expression::String(s)) => Some(s.value().as_str()),
        _ => None,
    }
}

/// Reduce a provider address or short name to its lowercased
/// `namespace/name`. Drops any registry host prefix and an implicit
/// `hashicorp` namespace for bare names: `aws` → `hashicorp/aws`,
/// `registry.terraform.io/hashicorp/aws` → `hashicorp/aws`.
fn provider_ns_name(s: &str) -> String {
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [name] => format!("hashicorp/{}", name.to_ascii_lowercase()),
        [.., ns, name] => format!("{}/{}", ns.to_ascii_lowercase(), name.to_ascii_lowercase()),
        _ => s.to_ascii_lowercase(),
    }
}

/// Whether a `required_providers` entry refers to the gate's provider,
/// matched by canonical source (explicit `source`, else the implicit
/// `hashicorp/<local key>`) rather than the local key name.
fn entry_matches_provider(local_key: &str, explicit_source: Option<&str>, canonical: &str) -> bool {
    let resolved = match explicit_source {
        Some(src) => provider_ns_name(src),
        None => provider_ns_name(local_key),
    };
    resolved == canonical
}

/// Extract the `source = "..."` attribute declared for
/// `provider_name` in any `terraform { required_providers { ... } }`
/// block. Only the long form carries a `source`; short-form
/// entries (`aws = "~> 4.0"`) implicitly resolve to
/// `hashicorp/<name>` per Terraform's own rule, so callers should
/// fall back to that default on `None`.
pub fn extract_required_provider_source(body: &Body, provider_name: &str) -> Option<String> {
    for structure in body.iter() {
        let Some(tf_block) = structure.as_block() else {
            continue;
        };
        if tf_block.ident.as_str() != "terraform" {
            continue;
        }
        for inner in tf_block.body.iter() {
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
                if attr.key.as_str() != provider_name {
                    continue;
                }
                if let Expression::Object(obj) = &attr.value {
                    for (k, v) in obj.iter() {
                        let key_matches = match k {
                            hcl_edit::expr::ObjectKey::Ident(id) => id.as_str() == "source",
                            hcl_edit::expr::ObjectKey::Expression(Expression::Variable(var)) => {
                                var.value().as_str() == "source"
                            }
                            hcl_edit::expr::ObjectKey::Expression(Expression::String(s)) => {
                                s.value().as_str() == "source"
                            }
                            _ => false,
                        };
                        if key_matches {
                            if let Some(s) = read_string_value(v.expr()) {
                                return Some(s);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn read_string_value(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(s.as_str().to_string()),
        Expression::StringTemplate(t) => {
            let mut acc = String::new();
            for element in t.iter() {
                match element {
                    hcl_edit::template::Element::Literal(lit) => acc.push_str(lit.as_str()),
                    _ => return None,
                }
            }
            Some(acc)
        }
        _ => None,
    }
}

fn label_str(label: &BlockLabel) -> Option<&str> {
    match label {
        BlockLabel::String(s) => Some(s.value().as_str()),
        BlockLabel::Ident(i) => Some(i.as_str()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tfls_parser::parse_source;

    const TEST_RULE: DeprecationRule = DeprecationRule {
        block_kind: "resource",
        label: "null_resource",
        gate: Gate::TerraformVersion { threshold: "1.4.0" },
        message: "test message",
    };

    const TEST_PROVIDER_RULE: DeprecationRule = DeprecationRule {
        block_kind: "resource",
        label: "aws_alb",
        gate: Gate::ProviderVersion {
            provider: "aws",
            threshold: "1.7.0",
        },
        message: "test provider message",
    };

    fn diags(src: &str) -> Vec<Diagnostic> {
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        diagnostics(&TEST_RULE, &body, &rope)
    }

    #[test]
    fn fires_when_unconstrained() {
        let d = diags("resource \"null_resource\" \"x\" {}\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].message, "test message");
    }

    #[test]
    fn ignores_unrelated_blocks() {
        let d = diags("resource \"aws_instance\" \"x\" { ami = \"a\" }\n");
        assert!(d.is_empty());
    }

    #[test]
    fn supports_predicate_min_threshold() {
        assert!(supports(&TEST_RULE, ">= 1.4"));
        assert!(supports(&TEST_RULE, "~> 1.5"));
        assert!(!supports(&TEST_RULE, ">= 1.0"));
        assert!(!supports(&TEST_RULE, "= 1.3.5"));
        assert!(!supports(&TEST_RULE, "< 1.3"));
    }

    #[test]
    fn for_module_overrides_body_gate() {
        let src = concat!(
            "terraform { required_version = \"< 1.3\" }\n",
            "resource \"null_resource\" \"x\" {}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        // `supports = true` overrides the local `< 1.3` body gate.
        let d = diagnostics_for_module(&TEST_RULE, &body, &rope, true);
        assert_eq!(d.len(), 1);
        let d = diagnostics_for_module(&TEST_RULE, &body, &rope, false);
        assert!(d.is_empty());
    }

    #[test]
    fn extract_required_version_reads_string_form() {
        let src = "terraform { required_version = \">= 1.4\" }\n";
        let body = parse_source(src).body.expect("parses");
        assert_eq!(extract_required_version(&body), Some(">= 1.4".into()));
    }

    #[test]
    fn extract_required_version_returns_none_when_absent() {
        let body = parse_source("resource \"x\" \"y\" {}\n")
            .body
            .expect("parses");
        assert!(extract_required_version(&body).is_none());
    }

    #[test]
    fn provider_gate_uses_required_providers_short_form() {
        let src = concat!(
            "terraform {\n  required_providers {\n    aws = \"~> 4.0\"\n  }\n}\n",
            "resource \"aws_alb\" \"x\" {}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = diagnostics(&TEST_PROVIDER_RULE, &body, &rope);
        assert_eq!(d.len(), 1, "fires when constraint admits >= 1.7");
    }

    #[test]
    fn provider_gate_uses_required_providers_long_form() {
        let src = concat!(
            "terraform {\n  required_providers {\n",
            "    aws = { source = \"hashicorp/aws\", version = \"~> 5.0\" }\n",
            "  }\n}\n",
            "resource \"aws_alb\" \"x\" {}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = diagnostics(&TEST_PROVIDER_RULE, &body, &rope);
        assert_eq!(d.len(), 1, "fires under long form too");
    }

    #[test]
    fn provider_gate_suppresses_when_constraint_excludes_threshold() {
        let src = concat!(
            "terraform {\n  required_providers {\n    aws = \"< 1.5\"\n  }\n}\n",
            "resource \"aws_alb\" \"x\" {}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = diagnostics(&TEST_PROVIDER_RULE, &body, &rope);
        assert!(d.is_empty(), "1.5 ceiling excludes 1.7+");
    }

    #[test]
    fn provider_gate_fires_when_no_required_providers_block() {
        // Absence of evidence — can't suppress.
        let src = "resource \"aws_alb\" \"x\" {}\n";
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = diagnostics(&TEST_PROVIDER_RULE, &body, &rope);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn provider_gate_ignores_other_provider_constraints() {
        // `random` constraint shouldn't gate the AWS rule.
        let src = concat!(
            "terraform {\n  required_providers {\n    random = \"< 1.0\"\n  }\n}\n",
            "resource \"aws_alb\" \"x\" {}\n",
        );
        let rope = Rope::from_str(src);
        let body = parse_source(src).body.expect("parses");
        let d = diagnostics(&TEST_PROVIDER_RULE, &body, &rope);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn extract_required_provider_version_short_form() {
        let body =
            parse_source("terraform {\n  required_providers {\n    aws = \"~> 4.0\"\n  }\n}\n")
                .body
                .expect("parses");
        assert_eq!(
            extract_required_provider_version(&body, "aws"),
            Some("~> 4.0".into())
        );
    }

    #[test]
    fn extract_required_provider_version_long_form() {
        let body = parse_source(concat!(
            "terraform {\n  required_providers {\n",
            "    aws = { source = \"hashicorp/aws\", version = \"5.42.0\" }\n",
            "  }\n}\n",
        ))
        .body
        .expect("parses");
        assert_eq!(
            extract_required_provider_version(&body, "aws"),
            Some("5.42.0".into())
        );
    }

    #[test]
    fn extract_required_provider_version_matches_aliased_local_by_source() {
        // Provider declared under a non-canonical local key but with the
        // canonical source — must still resolve for the `aws` gate.
        let body = parse_source(concat!(
            "terraform {\n  required_providers {\n",
            "    awscloud = { source = \"hashicorp/aws\", version = \"5.42.0\" }\n",
            "  }\n}\n",
        ))
        .body
        .expect("parses");
        assert_eq!(
            extract_required_provider_version(&body, "aws"),
            Some("5.42.0".into())
        );
    }

    #[test]
    fn extract_required_provider_version_excludes_fork_source() {
        // Local key `aws` but a forked source — the hashicorp/aws gate
        // must NOT match it.
        let body = parse_source(concat!(
            "terraform {\n  required_providers {\n",
            "    aws = { source = \"mycorp/aws\", version = \"5.42.0\" }\n",
            "  }\n}\n",
        ))
        .body
        .expect("parses");
        assert_eq!(extract_required_provider_version(&body, "aws"), None);
    }

    #[test]
    fn extract_required_provider_version_host_qualified_source() {
        let body = parse_source(concat!(
            "terraform {\n  required_providers {\n",
            "    aws = { source = \"registry.terraform.io/hashicorp/aws\", version = \"5.42.0\" }\n",
            "  }\n}\n",
        ))
        .body
        .expect("parses");
        assert_eq!(
            extract_required_provider_version(&body, "aws"),
            Some("5.42.0".into())
        );
    }

    #[test]
    fn extract_required_provider_source_long_form() {
        let body = parse_source(concat!(
            "terraform {\n  required_providers {\n",
            "    aws = { source = \"hashicorp/aws\", version = \"5.42.0\" }\n",
            "  }\n}\n",
        ))
        .body
        .expect("parses");
        assert_eq!(
            extract_required_provider_source(&body, "aws"),
            Some("hashicorp/aws".into())
        );
    }

    #[test]
    fn extract_required_provider_source_returns_none_for_short_form() {
        let body =
            parse_source("terraform {\n  required_providers {\n    aws = \"~> 4.0\"\n  }\n}\n")
                .body
                .expect("parses");
        assert!(extract_required_provider_source(&body, "aws").is_none());
    }

    #[test]
    fn supports_with_lock_lock_above_threshold_fires() {
        let v = semver::Version::new(5, 50, 0);
        assert!(supports_with_lock(
            &TEST_PROVIDER_RULE,
            Some("~> 4.0"),
            Some(&v),
        ));
    }

    #[test]
    fn supports_with_lock_lock_below_threshold_suppresses() {
        // Threshold is 1.7.0; locked at 1.5.0 — not yet upgraded.
        let v = semver::Version::new(1, 5, 0);
        assert!(!supports_with_lock(
            &TEST_PROVIDER_RULE,
            Some("~> 1.5"),
            Some(&v),
        ));
    }

    #[test]
    fn supports_with_lock_falls_back_to_constraint_when_no_lock() {
        // Constraint admits >= 1.7 — fires.
        assert!(supports_with_lock(
            &TEST_PROVIDER_RULE,
            Some("~> 1.7"),
            None,
        ));
        // Constraint excludes >= 1.7 — suppresses.
        assert!(!supports_with_lock(
            &TEST_PROVIDER_RULE,
            Some("< 1.5"),
            None,
        ));
    }

    #[test]
    fn supports_with_lock_no_inputs_fires_by_default() {
        // Absence of evidence — can't suppress.
        assert!(supports_with_lock(&TEST_PROVIDER_RULE, None, None));
    }

    #[test]
    fn supports_with_lock_terraform_version_rule_ignores_lock() {
        // TerraformVersion gates aren't covered by .terraform.lock.hcl;
        // a locked provider version must NOT influence them.
        let v = semver::Version::new(99, 0, 0);
        // Constraint excludes the 1.4 threshold; lock should be ignored.
        assert!(!supports_with_lock(&TEST_RULE, Some("< 1.3"), Some(&v)));
        // Constraint admits the threshold; fires.
        assert!(supports_with_lock(&TEST_RULE, Some(">= 1.4"), Some(&v)));
    }

    #[test]
    fn supports_with_lock_handles_two_part_threshold() {
        // The AWS rule's threshold is "1.7.0"; some rules use "4.0"
        // (two-part). Verify the parsing fallback handles it.
        const TWO_PART: DeprecationRule = DeprecationRule {
            block_kind: "resource",
            label: "x",
            gate: Gate::ProviderVersion {
                provider: "p",
                threshold: "4.0",
            },
            message: "m",
        };
        let v = semver::Version::new(4, 50, 0);
        assert!(supports_with_lock(&TWO_PART, None, Some(&v)));
        let v = semver::Version::new(3, 99, 99);
        assert!(!supports_with_lock(&TWO_PART, None, Some(&v)));
    }

    #[test]
    fn extract_required_provider_source_returns_none_when_provider_absent() {
        let body = parse_source(
            "terraform {\n  required_providers {\n    random = { source = \"hashicorp/random\" }\n  }\n}\n",
        )
        .body
        .expect("parses");
        assert!(extract_required_provider_source(&body, "aws").is_none());
    }
}
