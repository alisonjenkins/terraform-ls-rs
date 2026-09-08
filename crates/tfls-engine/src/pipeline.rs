//! The diagnostics pipeline: aggregates every diagnostic family
//! (syntax, undefined-reference, schema validation, deprecations,
//! style, ...) for a single document.
//!
//! Transport-free — reads only `StateStore` / `DocumentState`, no
//! `Backend` / tower-lsp dependency. `tfls-lsp`'s `did_open` /
//! `did_change` / `did_save` handlers call [`compute_diagnostics`]
//! and publish the result; the bulk workspace scan calls
//! [`compute_diagnostics_with_lookup`] with a precomputed
//! [`crate::snapshot::ModuleSnapshot`] instead of rebuilding the
//! cross-file aggregates per file.

use lsp_types::Diagnostic;
use tfls_core::SymbolKind;
use tfls_diag::{diagnostics_for_parse_errors, undefined_reference_diagnostics};
use tfls_parser::ReferenceKind;
use tfls_state::{StateStore, SymbolKey};
use url::Url;

use crate::module::{
    module_constraint_for_provider, module_locked_provider_version,
    module_providers_with_default_tags, module_supports_locals_replacement,
    module_supports_templatefile, module_supports_terraform_data, StateStoreSchemaLookup,
};

/// Compute the full diagnostic set for a document: syntax errors,
/// undefined-reference warnings, and schema validation errors.
///
/// Builds a fresh [`ModuleGraphAdapter`] per call — fine for
/// single-doc edits but O(N²) when called for every file in a
/// workspace scan. The bulk-scan path uses
/// [`compute_diagnostics_with_lookup`] with a precomputed snapshot
/// instead.
pub fn compute_diagnostics(state: &StateStore, uri: &Url) -> Vec<Diagnostic> {
    let module_dir = crate::module::parent_dir(uri);
    let graph = ModuleGraphAdapter {
        state,
        module_dir: module_dir.as_deref(),
        current_uri: uri,
    };
    let current_file = uri
        .path_segments()
        .and_then(|mut it| it.next_back())
        .unwrap_or("")
        .to_string();
    compute_diagnostics_with_lookup(state, uri, &graph, &current_file)
}

/// Same as [`compute_diagnostics`] but takes an injected
/// [`tfls_diag::ModuleGraphLookup`]. Lets the bulk-scan path reuse a
/// cached [`crate::snapshot::ModuleSnapshot`]
/// across every URI in a module instead of rebuilding the aggregates
/// per file.
pub fn compute_diagnostics_with_lookup(
    state: &StateStore,
    uri: &Url,
    graph: &dyn tfls_diag::ModuleGraphLookup,
    current_file: &str,
) -> Vec<Diagnostic> {
    let Some(doc) = state.documents.get(uri) else {
        return Vec::new();
    };

    let mut out = tag(
        "terraform_syntax",
        diagnostics_for_parse_errors(&doc.parsed.errors),
    );

    // Desync diagnostic aid: when the live parse failed, dump the server's
    // view of the buffer (version, byte length, full text). A "stuck syntax
    // error" report is almost always the server's rope having drifted from
    // the editor's buffer — out-of-order incremental edits (see
    // `DocumentState::apply_versioned_changes`) or a dropped edit. Compare
    // this dump against the editor's on-screen text to confirm: if they
    // differ, it's a desync; if they match, the syntax error is real.
    // Gated to TRACE so it's free in normal operation; enable with
    // `RUST_LOG=tfls_lsp::handlers::document=trace`.
    if !doc.parsed.errors.is_empty() && tracing::enabled!(tracing::Level::TRACE) {
        let text = doc.rope.to_string();
        tracing::trace!(
            uri = %uri,
            version = doc.version,
            bytes = text.len(),
            errors = ?doc.parsed.errors,
            "syntax-error rope dump (compare against editor buffer to spot desync):\n{text}"
        );
    }

    // Unformatted-file check against the active style. Body-independent
    // (reuses the cached format scan); a no-op when already formatted or
    // when the formatter can't parse the file.
    let fmt_style = state.config.snapshot().format_style;
    if let Some(fmt) = crate::format_scan::formatting_diagnostic(&doc, fmt_style) {
        out.extend(tag("terraform_fmt", vec![fmt]));
    }

    // Test files (`.tftest.hcl` / `.tftest.json`) parse like HCL but are NOT
    // module config: their references resolve against the module under test
    // (the dir alongside, or the parent when they live in `tests/`), and the
    // module-only ruleset below (schema validation, unused declarations,
    // deprecations, …) does not apply to them. Only undefined-ref + the
    // dedicated structural validator run.
    let is_test = tfls_core::uri::is_tftest_uri(uri.as_str());
    let module_dir = if is_test {
        crate::module::module_under_test_dir(uri)
    } else {
        crate::module::parent_dir(uri)
    };
    // Undefined-reference only on a CLEAN parse. While the file has a syntax
    // error its references come from the lenient text fallback, which happily
    // picks up a half-typed `local.region_short_na` and flags it as
    // undefined — a transient false positive that, if it gets published and
    // the next recompute is missed, sticks. Recovery also blanks broken
    // lines, so a real declaration can momentarily vanish and make a valid
    // reference look undefined. Suppress until the file parses cleanly (the
    // syntax-error diagnostic still fires); this mirrors the gating that
    // `unused_declarations` and schema validation already use.
    if !doc.parsed.has_errors() {
        out.extend(tag(
            "terraform_undefined_reference",
            undefined_reference_diagnostics(&doc.references, |kind| {
                // Resolve same-file references against THIS document's own
                // symbol table before the global index. We already hold the
                // doc read guard, so `doc.symbols` is a consistent snapshot;
                // the global `definitions_by_name` is not — a concurrent
                // `apply_and_reparse_document` clears this doc's entries
                // (`remove_from_indexes` runs under a *shared* `get`, so it
                // proceeds while this compute holds the read guard) before
                // `add_to_indexes` restores them. Without the own-symbols
                // fallback, an overlapping compute observes that empty window
                // and flags the file's own locals/vars/modules as undefined,
                // then clears on the next pass — the "locals flash undefined
                // on rapid edits" race. The current doc is in the module, so
                // its own declaration always satisfies module scope: this is
                // strictly sound (never a false negative).
                defined_in_current_doc(&doc.symbols, kind)
                    || is_defined_in_module(state, module_dir.as_deref(), kind)
            }),
        ));
    }

    // Body-based diagnostics run only on a CLEAN parse. When the parse
    // carried a syntax error the stored body is a best-effort recovery (its
    // broken lines blanked out by `parse_source_recovering`); validating it
    // would mistake a blanked-out `ami = …` for a missing required attribute.
    // Hover/completion still use the recovered body — that's the point — but
    // diagnostics stay exactly as they were before recovery existed.
    // The structural validator is the ONLY body-based rule that runs on a
    // test file; the module-only ruleset below is gated off (`&& !is_test`).
    if is_test {
        if let Some(body) = doc
            .parsed
            .body
            .as_ref()
            .filter(|_| !doc.parsed.has_errors())
        {
            out.extend(tag(
                "terraform_tftest",
                tfls_diag::tftest_diagnostics(body, &doc.rope),
            ));
        }
    }
    if let Some(body) = doc
        .parsed
        .body
        .as_ref()
        .filter(|_| !doc.parsed.has_errors() && !is_test)
    {
        let lookup = StateStoreSchemaLookup { state };
        let hints = RegistryDocsHints { state };
        out.extend(tag(
            "terraform_schema_validation",
            tfls_diag::schema_validation::resource_diagnostics_with_hints(
                body,
                &doc.rope,
                uri,
                &lookup,
                Some(&hints),
            ),
        ));
        // Untagged-resource warnings. Generic rule is schema-driven and
        // skips any provider that sets `default_tags` (those tags
        // auto-apply, aggregated across module siblings). The Name-tag
        // rule is schema-free (curated AWS table) so it fires even before
        // schemas are fetched.
        let dt_suppressed = module_providers_with_default_tags(state, uri);
        out.extend(tag(
            "terraform_missing_tags",
            tfls_diag::missing_tags_diagnostics(body, &doc.rope, &lookup, &dt_suppressed),
        ));
        out.extend(tag(
            "terraform_missing_name_tag",
            tfls_diag::missing_name_tag_diagnostics(body, &doc.rope),
        ));
        let cache_lookup = OnDiskVersionCache;
        out.extend(tag(
            "terraform_constraint",
            tfls_diag::constraint_diagnostics(body, &doc.rope, &cache_lookup),
        ));
        out.extend(tag(
            "terraform_variable_default_type",
            tfls_diag::variable_default_type_diagnostics(body, &doc.rope),
        ));
        // Pass the module-graph lookup so typed-variables can
        // suppress its warning on variables that are ALSO
        // unused — fixing the type on a soon-to-be-deleted
        // variable wastes the user's time. Lookup is only
        // consulted on root modules, matching
        // `unused_declarations`'s own gating.
        out.extend(tag(
            "terraform_typed_variables",
            tfls_diag::typed_variables_diagnostics(body, &doc.rope, Some(graph)),
        ));
        out.extend(tag(
            "terraform_module_version_presence",
            tfls_diag::module_version_presence_diagnostics(body, &doc.rope),
        ));
        out.extend(tag(
            "terraform_module_pinned_source",
            tfls_diag::module_pinned_source_diagnostics(body, &doc.rope),
        ));
        out.extend(tag(
            "terraform_module_shallow_clone",
            tfls_diag::module_shallow_clone_diagnostics(body, &doc.rope),
        ));
        out.extend(tag(
            "terraform_module_mutable_ref",
            tfls_diag::module_mutable_ref_diagnostics(body, &doc.rope),
        ));
        // Mismatch + outdated read the git-refs cache the prefetch job warms
        // (offline; cold cache ⇒ no diagnostic).
        out.extend(tag(
            "terraform_module_ref_tag_mismatch",
            tfls_diag::module_ref_tag_mismatch_diagnostics(body, &doc.rope, &|src, tag| {
                tfls_provider_protocol::git_refs::read_cached_repo_tags(src).and_then(|t| {
                    tfls_provider_protocol::git_refs::tag_to_sha(&t, tag).map(str::to_string)
                })
            }),
        ));
        out.extend(tag(
            "terraform_module_outdated",
            tfls_diag::module_outdated_diagnostics(body, &doc.rope, &|src| {
                tfls_provider_protocol::git_refs::read_cached_repo_tags(src)
                    .map(|t| tfls_provider_protocol::git_refs::tag_names(&t))
            }),
        ));
        out.extend(tag(
            "terraform_workspace_remote",
            tfls_diag::workspace_remote_diagnostics(body, &doc.rope),
        ));
        out.extend(tag(
            "terraform_deprecated_index",
            tfls_diag::deprecated_index_diagnostics(body, &doc.rope),
        ));
        out.extend(tag(
            "terraform_deprecated_interpolation",
            tfls_diag::deprecated_interpolation_diagnostics(body, &doc.rope),
        ));
        out.extend(tag(
            "terraform_deprecated_lookup",
            tfls_diag::deprecated_lookup_diagnostics(body, &doc.rope),
        ));
        // Module-aware gating: a `terraform { required_version }`
        // block typically lives in `versions.tf`, not the file we're
        // scanning, so we aggregate every sibling's constraint before
        // deciding whether to flag `null_resource` / `template_file`
        // blocks here.
        let null_resource_supported = module_supports_terraform_data(state, uri);
        out.extend(tag(
            "terraform_deprecated_null_resource",
            tfls_diag::deprecated_null_resource_diagnostics_for_module(
                body,
                &doc.rope,
                null_resource_supported,
            ),
        ));
        let templatefile_supported = module_supports_templatefile(state, uri);
        out.extend(tag(
            "terraform_deprecated_template_file",
            tfls_diag::deprecated_template_file_diagnostics_for_module(
                body,
                &doc.rope,
                templatefile_supported,
            ),
        ));
        out.extend(tag(
            "terraform_deprecated_template_dir",
            tfls_diag::deprecated_template_dir_diagnostics_for_module(
                body,
                &doc.rope,
                templatefile_supported,
            ),
        ));
        let locals_supported = module_supports_locals_replacement(state, uri);
        out.extend(tag(
            "terraform_deprecated_null_data_source",
            tfls_diag::deprecated_null_data_source_diagnostics_for_module(
                body,
                &doc.rope,
                locals_supported,
            ),
        ));
        // Provider-version-gated rule tables. Per provider:
        // pull module-aggregated `required_providers.<name>.version`
        // once, build a `rule_supported` closure that tests each
        // rule's threshold against that single string, dispatch
        // through the multi-rule body walker. Pattern repeats
        // per provider — captured by `run_provider_table` below.
        let aws_constraint = module_constraint_for_provider(state, uri, "aws");
        let aws_locked = module_locked_provider_version(state, uri, "aws");
        out.extend(tag(
            "terraform_aws_renames",
            tfls_diag::aws_renames_diagnostics_for_module(
                body,
                &doc.rope,
                &provider_rule_filter(&aws_constraint, aws_locked.as_ref()),
            ),
        ));
        let kubernetes_constraint = module_constraint_for_provider(state, uri, "kubernetes");
        let kubernetes_locked = module_locked_provider_version(state, uri, "kubernetes");
        out.extend(tag(
            "terraform_kubernetes_renames",
            tfls_diag::kubernetes_renames_diagnostics_for_module(
                body,
                &doc.rope,
                &provider_rule_filter(&kubernetes_constraint, kubernetes_locked.as_ref()),
            ),
        ));
        let azurerm_constraint = module_constraint_for_provider(state, uri, "azurerm");
        let azurerm_locked = module_locked_provider_version(state, uri, "azurerm");
        out.extend(tag(
            "terraform_azurerm_blocks",
            tfls_diag::azurerm_blocks_diagnostics_for_module(
                body,
                &doc.rope,
                &provider_rule_filter(&azurerm_constraint, azurerm_locked.as_ref()),
            ),
        ));
        let google_constraint = module_constraint_for_provider(state, uri, "google");
        let google_locked = module_locked_provider_version(state, uri, "google");
        out.extend(tag(
            "terraform_google_blocks",
            tfls_diag::google_blocks_diagnostics_for_module(
                body,
                &doc.rope,
                &provider_rule_filter(&google_constraint, google_locked.as_ref()),
            ),
        ));
        let vault_constraint = module_constraint_for_provider(state, uri, "vault");
        let vault_locked = module_locked_provider_version(state, uri, "vault");
        out.extend(tag(
            "terraform_vault_blocks",
            tfls_diag::vault_blocks_diagnostics_for_module(
                body,
                &doc.rope,
                &provider_rule_filter(&vault_constraint, vault_locked.as_ref()),
            ),
        ));
        out.extend(tag(
            "terraform_empty_list_equality",
            tfls_diag::empty_list_equality_diagnostics(body, &doc.rope),
        ));
        out.extend(tag(
            "terraform_map_duplicate_keys",
            tfls_diag::map_duplicate_keys_diagnostics(body, &doc.rope),
        ));
        // Same-file duplicate definitions (a hard `terraform validate`
        // error). Cross-file duplicates within a module are a separate,
        // index-driven follow-up.
        out.extend(tag(
            "terraform_duplicate_definition",
            tfls_diag::duplicate_definition_diagnostics(body, &doc.rope),
        ));
        // count/for_each meta-argument misuse.
        out.extend(tag(
            "terraform_meta_argument",
            tfls_diag::meta_argument_diagnostics(body, &doc.rope),
        ));
        // for_each/count whose key set depends on an apply-time value — a
        // plan-time error Terraform only surfaces during `terraform plan`.
        // `local.*` and resource/data configs are resolved module-wide: the
        // definitions usually live in a different file than the `for_each`
        // reading them.
        // Both unknown-value rules conservative-flag unresolved resource
        // references; while the module dir's scan is pending, a sibling file
        // may simply not be loaded yet — a transient false positive. Gate on
        // scan completion: mark_scan_completed triggers a diagnostics
        // refresh, so the rules appear moments later with full context.
        let unknown_rules_ready = module_dir
            .as_deref()
            .is_some_and(|dir| state.is_scan_completed(dir));
        let mut unknown_inputs = crate::module::module_unknown_inputs(state, uri);
        // Variables a CALLER passes apply-time values into (rebuilt by the
        // indexer after directory scans) — makes the child's
        // `for_each = var.x` flag with a caller-naming message.
        if let Some(dir) = module_dir.as_deref() {
            crate::module::fill_unknown_variables(state, dir, &mut unknown_inputs);
        }
        // module.<label>.<output> references resolve into the child module's
        // directory; the per-call cache bounds the cost to the number of
        // distinct referenced modules.
        let module_output_cache = crate::module::ModuleOutputCache::default();
        let module_output_resolver =
            module_dir
                .as_deref()
                .map(|dir| crate::module::ModuleOutputResolver {
                    state,
                    caller_dir: dir.to_path_buf(),
                    cache: &module_output_cache,
                });
        let module_outputs = module_output_resolver
            .as_ref()
            .map(|r| r as &dyn tfls_diag::unknown_value::ModuleOutputLookup);
        if unknown_rules_ready {
            out.extend(tag(
                "terraform_for_each_unknown_keys",
                tfls_diag::for_each_unknown_keys_diagnostics_with_ctx(
                    body,
                    &doc.rope,
                    &unknown_inputs,
                    Some(&lookup),
                    module_outputs,
                ),
            ));
            // import-block id / for_each requiring plan-known values
            // (Terraform 1.5+ config-driven import). Same unknown-value
            // analysis and module-wide inputs as the for_each rule.
            out.extend(tag(
                "terraform_import_unknown_id",
                tfls_diag::import_unknown_id_diagnostics_with_ctx(
                    body,
                    &doc.rope,
                    &unknown_inputs,
                    Some(&lookup),
                    module_outputs,
                ),
            ));
        }
        // Non-literal lifecycle arguments (a hard `terraform validate`
        // error: "Variables may not be used here").
        out.extend(tag(
            "terraform_lifecycle_literal",
            tfls_diag::lifecycle_literal_diagnostics(body, &doc.rope),
        ));
        // Dependency cycles among `local` values (a hard Terraform error).
        out.extend(tag(
            "terraform_cyclic_locals",
            tfls_diag::cyclic_locals_diagnostics(body, &doc.rope),
        ));
        // Sensitive variable leaking into a non-sensitive output. The
        // sensitive-variable set is aggregated across the module (vars
        // and outputs usually live in different files).
        let sensitive_vars = crate::module::module_sensitive_variables(state, uri);
        out.extend(tag(
            "terraform_sensitive_output",
            tfls_diag::sensitive_output_diagnostics(body, &doc.rope, &sensitive_vars),
        ));
        // Provider-defined function calls (Terraform 1.8+). Lives
        // outside `tfls-diag` because it needs `StateStore` access
        // for `required_providers` peer-walk + `state.functions`
        // lookup.
        out.extend(tag(
            "terraform_provider_function",
            crate::provider_fn::provider_function_call_diagnostics(state, uri, doc.value()),
        ));

        // Cross-file / module-scoped rules. `graph` is either the
        // fresh per-call adapter (from `compute_diagnostics`) or a
        // cached snapshot (from the bulk-scan path).
        out.extend(tag(
            "terraform_required_version_presence",
            tfls_diag::required_version_presence_diagnostics(body, &doc.rope, graph),
        ));
        // Lock-vs-constraint drift: user bumped a `version`
        // constraint but didn't `terraform init -upgrade` — the
        // lock file still pins the OLD version that no longer
        // satisfies the new constraint. Catch silently-broken
        // states before `terraform plan` chokes.
        out.extend(tag(
            "terraform_lock_constraint_drift",
            lock_vs_constraint_diagnostics(state, uri, body, &doc.rope),
        ));
        out.extend(tag(
            "terraform_required_providers_version",
            tfls_diag::required_providers_version_diagnostics(body, &doc.rope, graph),
        ));
        out.extend(tag(
            "terraform_unused_declarations",
            tfls_diag::unused_declarations_diagnostics(body, &doc.rope, graph),
        ));
        out.extend(tag(
            "terraform_unused_required_providers",
            tfls_diag::unused_required_providers_diagnostics(body, &doc.rope, graph),
        ));

        // Pass 3 — opt-in style pack. standard_module_structure belongs
        // here too: it warns on every variable/output when
        // variables.tf/outputs.tf is absent, i.e. on the common
        // single-file `main.tf` module, so it must not fire by default.
        if state.config.snapshot().style_rules {
            out.extend(tag(
                "terraform_standard_module_structure",
                tfls_diag::standard_module_structure_diagnostics(
                    body,
                    &doc.rope,
                    current_file,
                    graph,
                ),
            ));
            out.extend(tag(
                "terraform_documented_variables",
                tfls_diag::documented_variables_diagnostics(body, &doc.rope),
            ));
            out.extend(tag(
                "terraform_documented_outputs",
                tfls_diag::documented_outputs_diagnostics(body, &doc.rope),
            ));
            out.extend(tag(
                "terraform_naming_convention",
                tfls_diag::naming_convention_diagnostics(body, &doc.rope),
            ));
            out.extend(tag(
                "terraform_comment_syntax",
                tfls_diag::comment_syntax_diagnostics(&doc.rope),
            ));
        }
    }

    // Per-rule severity overrides + suppression (the `rules` config).
    // Applied before dedup so an `off` rule drops out entirely and a
    // remapped severity dedups on its final value.
    apply_rule_overrides(&mut out, &state.config.snapshot().rule_overrides);

    // Defensive dedup: same (range, severity, source, message)
    // tuple is by definition the same diagnostic. Some emission
    // sites can fire twice in pathological cases (e.g. peer-walk
    // hitting the active doc once via the active loop and once
    // via the iter_peers loop when state hasn't synced — rare,
    // but observed in the wild). A user who has two `terraform {
    // required_providers { rsa = ... } }` blocks at *different*
    // line offsets will still see two diagnostics: ranges differ,
    // dedup leaves both.
    {
        use lsp_types::DiagnosticSeverity;
        use rustc_hash::FxHashSet;
        type DedupKey = ((u32, u32, u32, u32), u8, String, String);
        // Map severity to its LSP numeric (0 = none) — `DiagnosticSeverity`
        // isn't `Hash`, and a `u8` avoids the per-diagnostic Debug-string
        // allocation the old key used in this hot loop.
        let sev_code = |s: Option<DiagnosticSeverity>| -> u8 {
            match s {
                Some(v) if v == DiagnosticSeverity::ERROR => 1,
                Some(v) if v == DiagnosticSeverity::WARNING => 2,
                Some(v) if v == DiagnosticSeverity::INFORMATION => 3,
                Some(v) if v == DiagnosticSeverity::HINT => 4,
                _ => 0,
            }
        };
        let pre = out.len();
        let mut seen: FxHashSet<DedupKey> = FxHashSet::default();
        out.retain(|d| {
            let r = (
                d.range.start.line,
                d.range.start.character,
                d.range.end.line,
                d.range.end.character,
            );
            let src = d.source.clone().unwrap_or_default();
            seen.insert((r, sev_code(d.severity), src, d.message.clone()))
        });
        if out.len() != pre {
            tracing::debug!(
                uri = %uri,
                dropped = pre - out.len(),
                kept = out.len(),
                "compute_diagnostics: dedup'd identical entries",
            );
        }
    }

    out
}

/// Set a stable rule `code` on every diagnostic that lacks one, then
/// return them. Used to wrap each rule's output so per-rule config can
/// target it. The first code wins (rules don't overwrite a code an inner
/// helper already set).
fn tag(code: &'static str, diags: Vec<lsp_types::Diagnostic>) -> Vec<lsp_types::Diagnostic> {
    diags
        .into_iter()
        .map(|mut d| {
            if d.code.is_none() {
                d.code = Some(lsp_types::NumberOrString::String(code.to_string()));
            }
            d
        })
        .collect()
}

/// Apply the user's per-rule severity overrides: drop diagnostics whose
/// rule is set to `off`, remap the severity of the rest. Diagnostics
/// without a code, or whose code has no override, pass through unchanged.
fn apply_rule_overrides(
    out: &mut Vec<lsp_types::Diagnostic>,
    overrides: &std::collections::HashMap<String, tfls_state::RuleSeverity>,
) {
    use tfls_state::RuleSeverity;
    if overrides.is_empty() {
        return;
    }
    out.retain_mut(|d| {
        let Some(lsp_types::NumberOrString::String(code)) = &d.code else {
            return true;
        };
        match overrides.get(code) {
            None => true,
            Some(RuleSeverity::Off) => false,
            Some(sev) => {
                d.severity = Some(match sev {
                    RuleSeverity::Hint => lsp_types::DiagnosticSeverity::HINT,
                    RuleSeverity::Info => lsp_types::DiagnosticSeverity::INFORMATION,
                    RuleSeverity::Warning => lsp_types::DiagnosticSeverity::WARNING,
                    RuleSeverity::Error => lsp_types::DiagnosticSeverity::ERROR,
                    RuleSeverity::Off => unreachable!("handled above"),
                });
                true
            }
        }
    });
}

/// Reads the already-populated on-disk caches used by the completion
/// path. Returning `None` suppresses the semantic no-match warning
/// (the completion fetch simply hasn't happened yet); returning a
/// `Vec<String>` lets `tfls-diag` compare user constraints against
/// actually-published versions.
struct OnDiskVersionCache;

impl tfls_diag::VersionCacheLookup for OnDiskVersionCache {
    fn cached_versions(&self, source: &tfls_diag::ConstraintSource) -> Option<Vec<String>> {
        match source {
            tfls_diag::ConstraintSource::TerraformCli => {
                // Cache directly under $XDG_CACHE_HOME/terraform-ls-rs/tool-versions/
                let path = tool_versions_cache_path("terraform")?;
                let tf = std::fs::read_to_string(&path).ok()?;
                let tofu_path = tool_versions_cache_path("opentofu")?;
                let tofu = std::fs::read_to_string(&tofu_path).ok();
                let mut out: Vec<String> = serde_json::from_str(&tf).ok()?;
                if let Some(tofu_body) = tofu {
                    if let Ok(extra) = serde_json::from_str::<Vec<String>>(&tofu_body) {
                        for v in extra {
                            if !out.contains(&v) {
                                out.push(v);
                            }
                        }
                    }
                }
                Some(out)
            }
            tfls_diag::ConstraintSource::Provider { namespace, name } => {
                let mut out = Vec::new();
                for registry in &["terraform", "opentofu"] {
                    let path = registry_versions_cache_path(registry, namespace, name)?;
                    if let Ok(body) = std::fs::read_to_string(&path) {
                        if let Ok(vs) = serde_json::from_str::<Vec<String>>(&body) {
                            for v in vs {
                                if !out.contains(&v) {
                                    out.push(v);
                                }
                            }
                        }
                    }
                }
                if out.is_empty() {
                    None
                } else {
                    Some(out)
                }
            }
            tfls_diag::ConstraintSource::Module {
                namespace,
                name,
                provider,
            } => {
                let mut out = Vec::new();
                for registry in &["terraform", "opentofu"] {
                    let path = module_versions_cache_path(registry, namespace, name, provider)?;
                    if let Ok(body) = std::fs::read_to_string(&path) {
                        if let Ok(vs) = serde_json::from_str::<Vec<String>>(&body) {
                            for v in vs {
                                if !out.contains(&v) {
                                    out.push(v);
                                }
                            }
                        }
                    }
                }
                if out.is_empty() {
                    None
                } else {
                    Some(out)
                }
            }
        }
    }
}

/// Walk `terraform { required_providers { ... } }` and emit a
/// warning for each provider whose declared `version` constraint
/// doesn't admit the lock-pinned version. Catches the case where
/// the user bumped a constraint (`~> 4.0` → `~> 4.71`) and forgot
/// to run `terraform init -upgrade` — `terraform plan` would
/// resolve the lock to the OLD version, which no longer satisfies
/// the new constraint, and fail at apply time. Surface it now
/// so the user sees the drift while editing.
fn lock_vs_constraint_diagnostics(
    state: &StateStore,
    uri: &Url,
    body: &hcl_edit::structure::Body,
    rope: &ropey::Rope,
) -> Vec<Diagnostic> {
    use hcl_edit::expr::{Expression, ObjectKey};
    use hcl_edit::repr::Span;
    use lsp_types::DiagnosticSeverity;
    let mut out = Vec::new();
    let Some(parent) = uri
        .to_file_path()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    else {
        return out;
    };
    let Some(lock) = state.lock_file_for(&parent) else {
        return out;
    };
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
                let provider_local = attr.key.as_str().to_string();
                let Expression::Object(obj) = &attr.value else {
                    continue;
                };
                let mut source_str: Option<String> = None;
                let mut version_lit: Option<(String, lsp_types::Range)> = None;
                for (key, value) in obj.iter() {
                    let key_str = match key {
                        ObjectKey::Ident(d) => d.as_str().to_string(),
                        ObjectKey::Expression(Expression::String(s)) => s.value().to_string(),
                        _ => continue,
                    };
                    match key_str.as_str() {
                        "source" => {
                            if let Expression::String(s) = value.expr() {
                                source_str = Some(s.value().to_string());
                            }
                        }
                        "version" => {
                            if let Expression::String(s) = value.expr() {
                                if let Some(span) = value.expr().span() {
                                    if let Ok(range) =
                                        tfls_parser::hcl_span_to_lsp_range(rope, span)
                                    {
                                        version_lit = Some((s.value().to_string(), range));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let Some((constraint_str, version_range)) = version_lit else {
                    continue;
                };
                let address = match source_str.as_deref() {
                    Some(s) => match tfls_core::ProviderAddress::parse(s) {
                        Ok(a) => a,
                        Err(_) => continue,
                    },
                    None => tfls_core::ProviderAddress::hashicorp(&provider_local),
                };
                let Some(lock_entry) = lock.get(&address) else {
                    continue;
                };
                let parsed = tfls_core::version_constraint::parse(&constraint_str);
                if !parsed.errors.is_empty() || parsed.constraints.is_empty() {
                    continue;
                }
                let lock_str = lock_entry.version.to_string();
                if tfls_core::version_constraint::satisfies_all(&parsed.constraints, &lock_str) {
                    continue;
                }
                out.push(Diagnostic {
                    range: version_range,
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("terraform-ls-rs".to_string()),
                    message: format!(
                        "version constraint `{constraint_str}` does not admit the \
                         lock-pinned version `{lock_str}` for `{provider_local}`. \
                         Run `terraform init -upgrade` to refresh the lock so \
                         `terraform plan/apply` matches the declared constraint."
                    ),
                    ..Default::default()
                });
            }
        }
    }
    out
}

/// Build a `rule_supported` closure for a provider table from
/// its module-aggregated constraint string + the
/// `.terraform.lock.hcl`-pinned version (if any). Caller threads
/// the result into `<provider>_diagnostics_for_module`.
///
/// Locked version is the source of truth when present — it's
/// what `terraform plan/apply` actually runs. The constraint is
/// the fallback when no lock file exists yet (workspace not
/// `terraform init`-ed). `None` for both ⇒ every rule fires
/// (absence of evidence).
fn provider_rule_filter<'a>(
    constraint: &'a Option<String>,
    locked: Option<&'a semver::Version>,
) -> impl Fn(&tfls_diag::deprecation_rule::DeprecationRule) -> bool + 'a {
    move |rule| tfls_diag::deprecation_rule::supports_with_lock(rule, constraint.as_deref(), locked)
}

fn cache_root_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("terraform-ls-rs"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            std::path::PathBuf::from(home)
                .join(".cache")
                .join("terraform-ls-rs"),
        );
    }
    None
}

fn sanitise(c: &str) -> String {
    c.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn tool_versions_cache_path(slug: &str) -> Option<std::path::PathBuf> {
    Some(
        cache_root_dir()?
            .join("tool-versions")
            .join(format!("{}.json", sanitise(slug))),
    )
}

fn registry_versions_cache_path(
    registry: &str,
    namespace: &str,
    name: &str,
) -> Option<std::path::PathBuf> {
    Some(
        cache_root_dir()?
            .join("registry-versions")
            .join(sanitise(registry))
            .join(sanitise(namespace))
            .join(sanitise(name))
            .join("versions.json"),
    )
}

fn module_versions_cache_path(
    registry: &str,
    namespace: &str,
    name: &str,
    provider: &str,
) -> Option<std::path::PathBuf> {
    Some(
        cache_root_dir()?
            .join("registry-versions")
            .join("modules")
            .join(sanitise(registry))
            .join(sanitise(namespace))
            .join(sanitise(name))
            .join(sanitise(provider))
            .join("versions.json"),
    )
}

/// True if the referencing document's OWN symbol table defines `kind`.
///
/// Consulted before the global module index (`is_defined_in_module`) so a
/// reference to a same-file `var.*` / `local.*` / `module.*` resolves even
/// while a concurrent reparse has transiently cleared this doc's entries
/// from `definitions_by_name`. The caller holds the doc read guard, so
/// `symbols` is a consistent snapshot; the global index is rebuilt
/// remove-then-add under `index_lock`, which the diagnostics reader does not
/// hold. The current doc is always in its own module, so an own-table hit
/// always satisfies module scope — strictly sound, never a false negative.
fn defined_in_current_doc(symbols: &tfls_core::SymbolTable, kind: &ReferenceKind) -> bool {
    match kind {
        ReferenceKind::Variable { name } => symbols.variables.contains_key(name),
        ReferenceKind::Local { name } => symbols.locals.contains_key(name),
        ReferenceKind::Module { name } => symbols.modules.contains_key(name),
        // resource / data-source refs are skipped upstream by the diag engine.
        _ => false,
    }
}

/// True if a definition for `kind` exists somewhere in the workspace index
/// with the same parent directory as the referencing document. Falls back to
/// a lenient `true` for URIs we can't resolve to a filesystem path, so
/// nonsense `file://` inputs don't spam diagnostics.
fn is_defined_in_module(
    state: &StateStore,
    module_dir: Option<&std::path::Path>,
    kind: &ReferenceKind,
) -> bool {
    let key = match kind {
        ReferenceKind::Variable { name } => SymbolKey::new(SymbolKind::Variable, name),
        ReferenceKind::Local { name } => SymbolKey::new(SymbolKind::Local, name),
        ReferenceKind::Module { name } => SymbolKey::new(SymbolKind::Module, name),
        // resource / data-source refs are skipped upstream by the diag engine.
        _ => return true,
    };
    let Some(locs) = state.definitions_by_name.get(&key) else {
        return false;
    };
    let Some(module_dir) = module_dir else {
        // Without a parseable parent dir we can't compare; treat as defined
        // to avoid false positives on exotic URIs.
        return !locs.is_empty();
    };
    locs.iter()
        .any(|loc| crate::module::location_in_dir(loc, module_dir))
}

/// Adapter that answers `UpgradeHintLookup` queries by reading the
/// latest-published-version registry-doc cache laid down by
/// `tfls_provider_protocol::registry_docs::fetch_latest_parsed_docs`.
///
/// All lookups are first-letter-prefix-based: a resource named
/// `azurerm_X` is assumed to belong to the `hashicorp/azurerm`
/// provider. That covers the common-providers map the rest of the
/// LSP relies on; community providers without a matching hashicorp
/// prefix won't get hints, which is the safe failure mode (no
/// false-positive recommendations to upgrade something we can't
/// identify).
struct RegistryDocsHints<'a> {
    state: &'a StateStore,
}

impl RegistryDocsHints<'_> {
    /// Resolve a resource / data-source type name to the
    /// `(namespace, name)` pair we'll consult the registry-doc
    /// cache for.
    ///
    /// Today this uses the `<provider_local>_<rest>` convention to
    /// pull the provider local name, then reuses the same map the
    /// completion path uses (`REQUIRED_PROVIDERS_COMMON_ENTRIES`)
    /// to resolve to a `(namespace, name)` pair. Limits hints to
    /// the curated set; out-of-set providers stay silent.
    fn resolve_provider(&self, type_name: &str) -> Option<(String, String, String)> {
        let local = type_name.split_once('_').map(|(p, _)| p)?;
        for (entry_local, source, _) in tfls_core::builtin_blocks::REQUIRED_PROVIDERS_COMMON_ENTRIES
        {
            if *entry_local != local {
                continue;
            }
            let (ns, name) = source.split_once('/')?;
            return Some((local.to_string(), ns.to_string(), name.to_string()));
        }
        None
    }

    fn make_hint(
        &self,
        local: String,
        ns: &str,
        name: &str,
        latest_version: String,
    ) -> tfls_diag::schema_validation::UpgradeHint {
        let installed = self
            .state
            .installed_version(&tfls_core::ProviderAddress::new(
                "registry.terraform.io",
                ns,
                name,
            ));
        tfls_diag::schema_validation::UpgradeHint {
            provider_local_name: local,
            latest_version,
            installed_version: installed,
        }
    }
}

impl tfls_diag::schema_validation::UpgradeHintLookup for RegistryDocsHints<'_> {
    fn attribute_hint(
        &self,
        type_name: &str,
        attr_name: &str,
    ) -> Option<tfls_diag::schema_validation::UpgradeHint> {
        let (local, ns, name) = self.resolve_provider(type_name)?;
        let cached = tfls_provider_protocol::registry_docs::cached_latest_parsed_docs(&ns, &name)?;
        // The doc cache stores top-level + nested attributes
        // flattened together (registry markdown reuses names
        // across nested blocks, so we lose the boundary at parse
        // time). Hint only when the attr appears in the
        // resource's top-level Argument Reference list — i.e. the
        // map is non-empty AND the attribute is in there. We use
        // direct membership; same-name nested attrs won't false-
        // positive too often in practice.
        let attrs = cached
            .resources
            .get(type_name)
            .or_else(|| cached.data_sources.get(type_name))?;
        if !attrs.contains_key(attr_name) {
            return None;
        }
        Some(self.make_hint(local, &ns, &name, cached.latest_version))
    }

    fn resource_hint(&self, type_name: &str) -> Option<tfls_diag::schema_validation::UpgradeHint> {
        let (local, ns, name) = self.resolve_provider(type_name)?;
        let cached = tfls_provider_protocol::registry_docs::cached_latest_parsed_docs(&ns, &name)?;
        if !cached.resources.contains_key(type_name) {
            return None;
        }
        Some(self.make_hint(local, &ns, &name, cached.latest_version))
    }

    fn data_source_hint(
        &self,
        type_name: &str,
    ) -> Option<tfls_diag::schema_validation::UpgradeHint> {
        let (local, ns, name) = self.resolve_provider(type_name)?;
        let cached = tfls_provider_protocol::registry_docs::cached_latest_parsed_docs(&ns, &name)?;
        if !cached.data_sources.contains_key(type_name) {
            return None;
        }
        Some(self.make_hint(local, &ns, &name, cached.latest_version))
    }
}

/// Adapter that answers the Pass 2 cross-file questions by reading
/// [`StateStore`]. Keyed on the document's own module directory so
/// references from *other* modules in the same workspace don't
/// mask an unused declaration here.
struct ModuleGraphAdapter<'a> {
    state: &'a StateStore,
    module_dir: Option<&'a std::path::Path>,
    current_uri: &'a Url,
}

impl ModuleGraphAdapter<'_> {
    fn has_ref(&self, key: &SymbolKey) -> bool {
        let Some(locs) = self.state.references_by_name.get(key) else {
            return false;
        };
        match self.module_dir {
            Some(dir) => locs
                .iter()
                .any(|loc| crate::module::location_in_dir(loc, dir)),
            None => !locs.is_empty(),
        }
    }
}

impl tfls_diag::ModuleGraphLookup for ModuleGraphAdapter<'_> {
    fn variable_is_referenced(&self, name: &str) -> bool {
        self.has_ref(&SymbolKey::new(SymbolKind::Variable, name))
    }

    fn local_is_referenced(&self, name: &str) -> bool {
        self.has_ref(&SymbolKey::new(SymbolKind::Local, name))
    }

    fn data_source_is_referenced(&self, type_name: &str, name: &str) -> bool {
        self.has_ref(&SymbolKey::resource(
            SymbolKind::DataSource,
            type_name,
            name,
        ))
    }

    fn used_provider_locals(&self) -> std::collections::HashSet<String> {
        // Provider local names are the prefix of resource types
        // (`aws_instance` → `aws`) plus any explicit local used via
        // `provider = foo.alias`. Walk every parsed document in the
        // same module dir to collect them.
        let mut used = std::collections::HashSet::new();
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            if let Some(dir) = self.module_dir {
                let doc_dir = crate::module::parent_dir(doc.key());
                if doc_dir.as_deref() != Some(dir) {
                    continue;
                }
            }
            collect_provider_locals(body, &mut used);
            crate::snapshot::collect_provider_function_locals(&doc.rope.to_string(), &mut used);
        }
        used
    }

    fn present_files(&self) -> std::collections::HashSet<String> {
        let Some(dir) = self.module_dir else {
            return std::collections::HashSet::new();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return std::collections::HashSet::new();
        };
        entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.to_string())
                    .filter(|s| s.ends_with(".tf") || s.ends_with(".tf.json"))
            })
            .collect()
    }

    fn is_primary_terraform_doc(&self) -> bool {
        // Primary = lexicographically-first URI in the same module
        // that contains at least one top-level `terraform {}` block.
        let mut candidates: Vec<String> = Vec::new();
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            if let Some(dir) = self.module_dir {
                let doc_dir = crate::module::parent_dir(doc.key());
                if doc_dir.as_deref() != Some(dir) {
                    continue;
                }
            }
            let has_tf_block = body.iter().any(|s| {
                s.as_block()
                    .is_some_and(|b| b.ident.as_str() == "terraform")
            });
            if has_tf_block {
                candidates.push(doc.key().as_str().to_string());
            }
        }
        candidates.sort();
        candidates
            .first()
            .map(|s| s.as_str() == self.current_uri.as_str())
            .unwrap_or(false)
    }

    fn module_has_required_version(&self) -> bool {
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            if let Some(dir) = self.module_dir {
                let doc_dir = crate::module::parent_dir(doc.key());
                if doc_dir.as_deref() != Some(dir) {
                    continue;
                }
            }
            for structure in body.iter() {
                let Some(block) = structure.as_block() else {
                    continue;
                };
                if block.ident.as_str() != "terraform" {
                    continue;
                }
                if block.body.iter().any(|s| {
                    s.as_attribute()
                        .is_some_and(|a| a.key.as_str() == "required_version")
                }) {
                    return true;
                }
            }
        }
        false
    }

    fn providers_with_version_set(&self) -> std::collections::HashSet<String> {
        use hcl_edit::expr::{Expression, ObjectKey};
        let mut out = std::collections::HashSet::new();
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            if let Some(dir) = self.module_dir {
                let doc_dir = crate::module::parent_dir(doc.key());
                if doc_dir.as_deref() != Some(dir) {
                    continue;
                }
            }
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
                        let name = attr.key.as_str();
                        let Expression::Object(obj) = &attr.value else {
                            continue;
                        };
                        let has_version = obj.iter().any(|(k, _v)| match k {
                            ObjectKey::Ident(id) => id.as_str() == "version",
                            ObjectKey::Expression(Expression::Variable(v)) => {
                                v.value().as_str() == "version"
                            }
                            ObjectKey::Expression(Expression::String(s)) => {
                                s.value().as_str() == "version"
                            }
                            _ => false,
                        });
                        if has_version {
                            out.insert(name.to_string());
                        }
                    }
                }
            }
        }
        out
    }

    fn is_root_module(&self) -> bool {
        // We're a root module if no `module { source = "..." }`
        // block in any other module resolves to our directory.
        // Cheap heuristic: check whether any indexed document's
        // body has a `module` block whose resolved source points
        // at our dir. Exact path resolution is handled elsewhere;
        // here we accept any hit as "not root" to keep the check
        // conservative.
        let Some(dir) = self.module_dir else {
            // Without a dir we can't tell — assume root (the user
            // probably opened a lone file).
            return true;
        };
        for doc in self.state.documents.iter() {
            let Some(body) = doc.parsed.body.as_ref() else {
                continue;
            };
            let doc_dir = crate::module::parent_dir(doc.key());
            // Skip documents in the same module — a module calling
            // itself isn't a concern, and intra-module `module`
            // blocks point at sub-dirs, not this dir.
            if doc_dir.as_deref() == Some(dir) {
                continue;
            }
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
                    if let hcl_edit::expr::Expression::String(s) = &attr.value {
                        let src = s.value().as_str();
                        if source_points_at(src, doc_dir.as_deref(), dir) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    fn is_applyable_root(&self) -> bool {
        self.is_root_module()
            && crate::snapshot::module_has_applyable_config(self.state, self.module_dir)
    }
}

/// Resolve a module `source = "..."` string relative to the calling
/// module's dir and check whether it points at `target`. Only
/// local-path sources are resolved; everything else (registry, git,
/// etc.) can't possibly point at a local workspace dir.
fn source_points_at(
    source: &str,
    caller_dir: Option<&std::path::Path>,
    target: &std::path::Path,
) -> bool {
    if !(source.starts_with("./") || source.starts_with("../") || source.starts_with('/')) {
        return false;
    }
    let Some(caller_dir) = caller_dir else {
        return false;
    };
    let resolved = caller_dir.join(source);
    // Normalise both paths for comparison.
    let resolved = match std::fs::canonicalize(&resolved) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let target = match std::fs::canonicalize(target) {
        Ok(p) => p,
        Err(_) => return false,
    };
    resolved == target
}

/// Walk a body collecting every provider local name used by
/// `resource`/`data` blocks (via resource-type prefix) and by
/// explicit `provider = foo.alias` attrs.
fn collect_provider_locals(
    body: &hcl_edit::structure::Body,
    out: &mut std::collections::HashSet<String>,
) {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        match block.ident.as_str() {
            "resource" | "data" => {
                if let Some(label) = block.labels.first() {
                    let type_name = match label {
                        hcl_edit::structure::BlockLabel::String(s) => s.value().as_str(),
                        hcl_edit::structure::BlockLabel::Ident(i) => i.as_str(),
                    };
                    if let Some(local) = type_name.split('_').next() {
                        if !local.is_empty() {
                            out.insert(local.to_string());
                        }
                    }
                }
                // `provider = foo.alias` inside the block body.
                for attr in block.body.iter().filter_map(|s| s.as_attribute()) {
                    if attr.key.as_str() == "provider" {
                        if let Some(local) = extract_provider_local(&attr.value) {
                            out.insert(local);
                        }
                    }
                }
            }
            "provider" => {
                if let Some(label) = block.labels.first() {
                    let name = match label {
                        hcl_edit::structure::BlockLabel::String(s) => {
                            s.value().as_str().to_string()
                        }
                        hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
                    };
                    out.insert(name);
                }
            }
            "module" => {
                // `providers = { aws = aws.useast1 }` passes this module's
                // provider config to a child — both sides count as used.
                use hcl_edit::expr::{Expression, ObjectKey};
                for attr in block.body.iter().filter_map(|s| s.as_attribute()) {
                    if attr.key.as_str() != "providers" {
                        continue;
                    }
                    let Expression::Object(obj) = &attr.value else {
                        continue;
                    };
                    for (key, val) in obj.iter() {
                        if let Some(local) = extract_provider_local(val.expr()) {
                            out.insert(local);
                        }
                        let key_ident = match key {
                            ObjectKey::Ident(id) => Some(id.as_str().to_string()),
                            ObjectKey::Expression(Expression::Variable(v)) => {
                                Some(v.value().as_str().to_string())
                            }
                            ObjectKey::Expression(Expression::String(s)) => {
                                Some(s.value().as_str().to_string())
                            }
                            _ => None,
                        };
                        if let Some(k) = key_ident {
                            out.insert(k);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Extract `foo` from a `provider = foo.alias` expression.
fn extract_provider_local(expr: &hcl_edit::expr::Expression) -> Option<String> {
    match expr {
        hcl_edit::expr::Expression::Variable(v) => Some(v.value().as_str().to_string()),
        hcl_edit::expr::Expression::Traversal(t) => {
            if let hcl_edit::expr::Expression::Variable(v) = &t.expr {
                Some(v.value().as_str().to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod undefined_reference_race_tests {
    //! Repro for the same-file-locals-flash race.
    //!
    //! `compute_diagnostics` resolves references against the global
    //! `definitions_by_name` index, read WITHOUT holding `index_lock`.
    //! A concurrent `did_change` handler's `apply_and_reparse_document`
    //! clears this doc's entries (`remove_from_indexes`, a *shared* `get`
    //! that runs while another compute already holds the doc read guard)
    //! before `add_to_indexes` restores them — so an overlapping compute
    //! sees the empty window and flags the file's OWN locals as undefined.
    //!
    //! The fix: resolve same-file references against the document's own
    //! symbol table (held consistent under the doc read guard) before
    //! consulting the global index. Here we simulate the transient window
    //! by clearing `definitions_by_name` while leaving the doc intact.

    use super::compute_diagnostics;
    use tfls_state::{DocumentState, StateStore};
    use url::Url;

    #[test]
    fn same_file_local_not_flagged_while_global_index_cleared() {
        let store = StateStore::new();
        let uri = Url::parse("file:///mod/main.tf").unwrap();
        store.upsert_document(DocumentState::new(
            uri.clone(),
            "locals {\n  a = 1\n}\noutput \"o\" {\n  value = local.a\n}\n",
            1,
        ));

        // Sanity: with the index populated there's no undefined-ref diag.
        let clean = compute_diagnostics(&store, &uri);
        assert!(
            !clean.iter().any(|d| d.message.contains("undefined local")),
            "baseline must resolve the same-file local: {clean:?}"
        );

        // Simulate the cleared-but-not-yet-repopulated window a concurrent
        // reparse opens. The doc (and its own symbol table) is untouched.
        store.definitions_by_name.clear();

        let racing = compute_diagnostics(&store, &uri);
        assert!(
            !racing
                .iter()
                .any(|d| d.message.contains("undefined local `a`")),
            "same-file local must resolve from the doc's own symbols even \
             while the global index is transiently empty: {racing:?}"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod provider_locals_tests {
    use super::collect_provider_locals;
    use std::collections::HashSet;

    fn locals(src: &str) -> HashSet<String> {
        let body = tfls_parser::parse_source(src).body.expect("parse");
        let mut out = HashSet::new();
        collect_provider_locals(&body, &mut out);
        out
    }

    #[test]
    fn module_providers_meta_arg_marks_local_used() {
        // A provider passed to a child via `providers = {}` must count
        // as used, else it trips a false unused-required-providers warning.
        let src = "module \"x\" {\n  source = \"./child\"\n  \
                   providers = {\n    aws = aws.useast1\n  }\n}\n";
        let used = locals(src);
        assert!(used.contains("aws"), "got: {used:?}");
    }

    #[test]
    fn module_providers_distinct_key_and_value_both_used() {
        let src = "module \"x\" {\n  source = \"./child\"\n  \
                   providers = {\n    kubernetes = kubernetes.useast1\n    aws = awsalt\n  }\n}\n";
        let used = locals(src);
        assert!(used.contains("kubernetes"), "got: {used:?}");
        assert!(used.contains("aws"), "got: {used:?}");
        assert!(used.contains("awsalt"), "got: {used:?}");
    }
}
