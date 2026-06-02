# Diagnostics Deep-Dive Backlog

Source: multi-agent diagnostics deep-dive (8 survey lenses → per-finding assess/verify → synthesize), 2026-06-02.
Workflow: `diagnostics-deep-dive`. 64 agents, ~3.1M tokens. Bugs adversarially refuted; features/improvements scored for value, effort, and non-duplication.

**Counts (after dedup): 13 bugs, 11 missing features, 14 improvements.** Effort = rough size (S/M/L); confidence from the assessing agent.

---

## Bugs

- [x] **Kubernetes `_v1` auto-fix produces non-existent type `kubernetes_daemonset_v1`** (high, effort S, confidence high) — `code_action_block_rename.rs:240` + `deprecated_kubernetes_renames.rs:48`
    `resolved_to` synthesises `<from>_v1`, yielding `kubernetes_daemonset_v1`, but the real registry type is `kubernetes_daemon_set_v1`. The quick-fix rewrites the block + refs to a type that does not exist, so the config fails `terraform validate`/`plan`.
    **Proposal:** Special-case `kubernetes_daemonset` → `kubernetes_daemon_set_v1` (or spell out `to` explicitly per spec, mirroring the AWS table). Other family members verified correct.

- [x] **variable_default_type emits ERROR for valid Terraform primitive coercion (string↔number↔bool)** (high, effort S, confidence high) — `variable_default_type.rs:52` (via `tfls-core/src/variable_type.rs:795`)
    `satisfies()` compares primitives by strict equality with no coercion table, so `type = number, default = "5"`, `type = string, default = 5`, `type = bool, default = "true"` — all accepted by `terraform plan` — are flagged as hard ERROR diagnostics.
    **Proposal:** Add Terraform's primitive conversion rules to the (Primitive, Primitive) arm: treat string↔number and string↔bool as compatible (worst case = a missed error, not a false positive). At minimum downgrade uncertain cases ERROR→WARNING.

- [x] **exactly_one_of never warns when ZERO members are set** (medium, effort S, confidence high) — `schema_validation.rs:358-376`
    The handler only fires when two members are both present; setting zero members passes silently, though Terraform requires exactly one. Asymmetric with at_least_one_of.
    **Proposal:** Add a per-group pass (mirroring at_least_one_of's `seen_groups`) counting present members, warn when count is 0; keep the existing >1 warning. Dedup by sorted group.

- [x] **Kubernetes rename gate threshold 2.0.0 is wrong; deprecation happens at provider 3.0.0** (medium, effort S, confidence high) — `deprecated_kubernetes_renames.rs:66` + `code_action_block_rename.rs:231`
    Rules gate on `kubernetes >= 2.0.0`, but unversioned types aren't deprecated until v3.0.0 and the `_v1` variants only exist from v2.7.0. On provider 2.0–2.6 the warning fires for fully-supported types and offers a migration to a nonexistent target.
    **Proposal:** Bump both thresholds to `3.0.0`; adjust message text "2.0+"→"3.0+".

- [x] **empty_list_equality emits wrong replacement + false semantic claim for the `!=` case** (medium, effort S, confidence high) — `empty_list_equality.rs:37-42`
    For `x != []` the message says "always false; use `length(x) >= 0`" — both wrong: `x != []` is always TRUE, and `length(x) >= 0` is vacuously always-true. Following the advice converts a meaningful inequality into a constant `true`. The `==` branch is correct.
    **Proposal:** Branch the message on the operator. For `!=`: "always true; use `length(x) > 0`". Regression test asserting the `!=` message contains `> 0` and `true`.

- [x] **Provider passed to a child module via `module { providers = {...} }` flagged as unused** (medium, effort S, confidence high) — `document.rs:1345` + `module_snapshot.rs:113-147`
    `used_provider_locals` never inspects the `providers = { x = x }` meta-argument on `module` blocks, so a root declaring a provider purely to pass it down gets a false "declared but not used" — on the recommended multi-provider composition pattern.
    **Proposal:** Add a `"module"` arm in both collection sites iterating the `providers` object, running `extract_provider_local` on key and value head idents, inserting both.

- [x] **Pre-release version ordering is string-compared, producing wrong constraint results** (medium, effort M, confidence high) — `tfls-core/src/version_constraint.rs:454-486`
    `VersionKey` derives Ord with `pre_id: String` last, so `1.0.0-rc10` byte-sorts below `1.0.0-rc2`. Any constraint crossing a numeric pre-release boundary can reach the wrong conclusion. Reachable via rc/beta releases.
    **Proposal:** Replace `pre_id: String` with `Vec<PreSegment{Num(u64),Text(String)}>` + semver-rule Ord (numeric compared numerically, numeric < alphanumeric, fewer fields < more).

- [x] **No-match version warning never fires for private registry sources; provider-source host dropped** (medium, effort M, confidence high) — `version_constraint.rs:345-369`
    `parse_provider_source` discards the host, so `app.terraform.io/org/foo` queries the PUBLIC registry for `org/foo`. `parse_module_source_parts` rejects 4-segment host-prefixed module sources entirely, inconsistent with module_version_presence.
    **Proposal:** Carry optional host through `ConstraintSource` into the cache key (separate or explicitly-skipped private catalogs). Accept the 4-segment host-prefixed module form.

- [x] **comment_syntax false-positives on `//` and `http://` inside heredocs** (medium, effort M, confidence high) — `comment_syntax.rs:12`
    The byte scanner has no heredoc state, so `//` inside `<<EOF ... EOF` bodies (URLs, shell, JS in user_data/command/policy) triggers a "use `#`" diagnostic pointing inside literal string data. Fires only when `style_rules` on, but a clear false positive.
    **Proposal:** Add heredoc tracking (capture terminator, suppress comment scanning until the terminator line, handle `<<-` and quoted terminators), or collect heredoc spans from the AST and skip those byte ranges.

- [x] **standard_module_structure fires by default and flags the common single-file root module** (medium, effort S, confidence high) — `standard_module_structure.rs:81`; ungated in `document.rs:576`
    NOT behind the `style_rules` opt-in (unlike siblings) and warns on every `variable`/`output` whenever variables.tf/outputs.tf is absent — i.e. on the common single-file `main.tf`. tflint keeps the equivalent in its opt-in `all` preset.
    **Proposal:** Gate behind `style_rules`, or only fire when the expected file exists elsewhere in the module (suppress the file-missing-entirely branch).

- [x] **didChangeConfiguration updates config but never re-publishes — live styleRules toggle no-ops** (medium, effort S, confidence high) — `workspace.rs:11-27`
    `did_change_configuration` calls `update_from_json` and returns without recomputing. Toggling `styleRules: true` produces zero new diagnostics until the user edits each buffer; stale style diagnostics persist after toggling off — breaking the documented live-toggle.
    **Proposal:** After `update_from_json`, call `maybe_refresh_diagnostics` + a push-mode republish over `state.open_docs`; optionally only when a diagnostic-affecting key changed.

- [x] **did_change_watched_files DELETED removes the doc but never clears its diagnostics nor refreshes peers** (medium, effort S, confidence high) — `workspace.rs:41-44`
    On delete the handler calls `remove_document` and stops: published diagnostics for the deleted file linger in the client forever, and sibling buffers keep stale reference resolution. Contrast `did_close`, which publishes an empty set.
    **Proposal:** On DELETED also publish an empty diagnostic set for the URI and enqueue a peer recompute for the parent dir.

- [x] **Provider-version gate matches on local provider name only, ignoring source** (low, effort M, confidence med) — `deprecation_rule.rs:397-448` + `util.rs:79`
    ProviderVersion gates resolve by local key name (literally `aws`), never `source`. An aliased local (`awscloud = { source = "hashicorp/aws" }`) misses the lookup; a local `aws` pointing at a fork is treated as canonical. The lock path resolves via source, so the two can disagree.
    **Proposal:** Match the rule's provider by canonical source address (hashicorp/<name> with short-form defaulting) so constraint and lock gates agree.

- [x] **Expression walker never visits computed object keys** (low, effort S, confidence high) — `expr_walk.rs:60-65`
    The `Expression::Object` arm recurses only into values, dropping `ObjectKey::Expression` keys. Every expr-walk rule misses patterns in computed-key position (`{ (lookup(var.m,"k")) = 1 }`), contradicting the module's "no expression position is missed".
    **Proposal:** `if let ObjectKey::Expression(k) = key { visit_expr(k, visit); }` before visiting the value. Add a test; correct the doc-comment.

- [x] **satisfies_all admits pre-release candidates that go-version would exclude** (low, effort S, confidence high) — `tfls-core/src/version_constraint.rs:353-425`
    A pre-release with a higher core than a stable constraint (`6.0.0-beta1` vs `>= 5.99.0`) satisfies it, though go-version rejects pre-releases unless the matching operand carries one on the same core. Since the cached registry list includes pre-releases, the "no published version matches" warning can be falsely suppressed.
    **Proposal:** In `satisfies_one`, when the candidate is a pre-release and the matched operand has none, return false unless the candidate's (major,minor,patch) core exactly equals the constraint's core.

---

## Missing features

- [x] **No duplicate-definition diagnostic (resource/variable/output/module/data with same address)** (high, effort M, confidence high) — `tfls-diag/` (no rule); `tfls-state/src/store.rs:76`
    `terraform validate` errors hard on duplicate declarations (a common copy-paste mistake); the server emits nothing until the CLI. `definitions_by_name` already stores per-(kind,name) location vecs (len>1 = duplicate).
    **Proposal:** Add `duplicate_definition_diagnostics`: same-file raw `body.iter()` scan (per-doc SymbolTable dedups same-file dups, so index-only misses them) + cross-file `definitions_by_name` lookup scoped to the module dir; ERROR on the label range. Keys: resource/data = (type,name); variable/output/module/local/provider-local = name.

- [x] **Sensitive value leaking into a non-sensitive output is not flagged** (high, effort M, confidence high) — `tfls-diag/` (no rule); sensitive flag at `tfls-schema/src/types.rs:70`
    Terraform errors when a `sensitive = true` variable or schema-sensitive attribute flows into an `output` not marked sensitive — a security-relevant gap. The server has the data but no rule correlates them.
    **Proposal:** Add `sensitive_output_diagnostics`: build a sensitive-source set (vars `sensitive = true`, schema-sensitive attr paths), walk each `output`, and if it lacks `sensitive = true` but references a sensitive source via expr_walk, emit ERROR + a `sensitive = true` quick-fix. Stage variable-half first; handle `nonsensitive(...)` and locals propagation.

- [x] **Required nested blocks (min_items >= 1) are never validated** (medium, effort M, confidence high) — `schema_validation.rs:287-300`
    The missing-required loop iterates only `schema.block.attributes`; `NestedBlockSchema.min_items`/`max_items` are never consulted, so omitting a mandatory nested block (or exceeding max_items) passes clean.
    **Proposal:** After the attribute loop, iterate `schema.block.block_types`, counting nested-block idents (a `dynamic "<label>"` satisfies min_items). Emit `missing required block` when min_items>=1 and count==0 with no matching dynamic; `too many "<name>" blocks (max N)` when count > max_items > 0.

- [x] **No attribute type checking / allowed_values enum validation despite schema carrying both** (enum + structural type-check) (medium, effort M, confidence high) — `schema_validation.rs` (validate_block); `types.rs:57,94`
    `AttributeSchema.r#type` and registry-mined `allowed_values` are populated but only power hover/completion. `instance_type = 5` or `volume_type = "gp9"` produces no diagnostic.
    **Proposal:** For literal-only values (skip traversals/templates to avoid FPs, and skip scalar string↔number/bool coercion), flag structural primitive/collection mismatches against the cty type, and flag string literals not in `allowed_values` when `Some` (WARNING — docs-mined enums lag). Pair the enum check with a nearest-valid-value quick-fix.

- [x] **No diagnostic for count/for_each misuse and `each.*`/`count.*`/`self.*` out of scope** (medium, effort M, confidence high) — `schema_validation.rs:242-247`; `references.rs:196`; `expr_walk.rs`
    `count`/`for_each` are skipped as meta-args with no validation, and each/count/self are blanket-excluded from reference classification, so three ERROR-class mistakes get no feedback: both count and for_each on one block; `for_each` over a list literal; `each.*`/`count.*`/`self.*` used where the enclosing block lacks the meta-arg/context.
    **Proposal:** In validate_block, flag both-count-and-for_each (ERROR on the second) and `for_each = [...]` (WARNING + `toset()` quick-fix). Add a context-threading body walker (not the flat helper) flagging each/count traversals whose enclosing block lacks the meta-arg, and self.* outside provisioner/connection/lifecycle; propagate scope into nested/dynamic blocks.

- [ ] **Used resource/data type has no matching required_providers entry (missing provider source)** (medium, effort M, confidence med) — `unused_required_providers.rs` (inverse exists, not this direction)
    The server flags unused required_providers but not the inverse: a resource/data whose provider local has no required_providers entry. For non-hashicorp providers this breaks `terraform init`; `resource "datadog_monitor"` with no source gets no warning.
    **Proposal:** Add `missing_required_provider_diagnostics`: a `declared_provider_locals()` accessor aggregating required_providers across siblings; for each used local not declared AND not a well-known hashicorp/builtin name, emit WARNING on the first such resource. Gate to root modules.

- [x] **depends_on entries must be bare resource/module refs, not arbitrary expressions — unvalidated** (medium, effort M, confidence high) — `references.rs:21`; `tfls-parser/src/references.rs:199`
    `depends_on` accepting arbitrary expressions (rather than bare `resource.name` / `module.name` references) goes unflagged.
    **Proposal:** Add the `depends_on`-must-be-bare-ref check alongside the each/count-out-of-scope walker; implement as a dedicated body walk since the parser drops each/count References.

- [x] **Style rules are an all-or-nothing toggle — no per-rule enable/disable** (medium, effort M, confidence high) — `tfls-state/src/config.rs:66`; `document.rs:584`
    `styleRules` is one bool flipping four rules together, while typed_variables/variable_default_type/standard_module_structure are hardwired on. Teams wanting one rule but not another (tflint's `rule "name" { enabled = ... }`) have no recourse.
    **Proposal:** Add a per-rule `rules: HashMap<&'static str, RuleSetting{enabled, severity}>` from initializationOptions/didChangeConfiguration, keep `styleRules` as a bulk default, consult the map at each emit site keyed by a new stable `code`. (Shares the stable-code prerequisite with per-rule severity.)

- [x] **No per-rule severity configuration — every diagnostic hardcodes its DiagnosticSeverity** (medium, effort L, confidence high) — `tfls-state/src/config.rs:45-71`; all rule files
    No way to remap severities (demote noisy rules to HINT/off, promote a deprecation to ERROR in CI), unlike terraform-ls and tflint.
    **Proposal:** Add `rule_overrides: HashMap<&'static str, RuleSetting>` (off/hint/info/warning/error) keyed by stable rule id; apply in a single post-pass in `compute_diagnostics_with_lookup` keyed off `source` + a new stable `code` field added to ~45 emit sites.

- [ ] **Diagnostic-only split families (azurerm/google/vault) could offer scaffolding code actions** (low, effort M, confidence high) — `deprecated_{azurerm,google,vault}_blocks.rs`
    azurerm VM split, google dataflow split, vault_generic_secret are diagnostic-only (target not auto-inferable) and give prose but nothing actionable. The framework already emits commented-out scaffolding (s3/k8s paths).
    **Proposal:** Add an Instance-scope action per split rule inserting commented-out skeletons of both candidate replacements with a "pick one, delete the other" header. Scaffold content is new code (existing helpers target `moved {}`, not resource skeletons); reuse the make_X_for_diag/at_cursor dispatch + gate plumbing.

- [ ] **No cyclic-reference detection for locals / modules** (low, effort L, confidence high) — `module_graph.rs` (graph exists for referenced-checks only)
    Terraform errors on dependency cycles (`Cycle: local.a, local.b`); self/mutually-referential locals/outputs are invisible. `ModuleGraphLookup` exposes only boolean "is referenced", not edges/topology.
    **Proposal:** Extend the snapshot to expose reference edges, run Tarjan/Kosaraju SCC over locals+outputs per module dir, emit ERROR per SCC>1 member or self-loop with a readable `a -> b -> a` path. Start with locals.

---

## Improvements

- [ ] **AWS `aws_alb*` alias family flagged deprecated though the provider does not mark it** (medium, effort M, confidence high) — `deprecated_aws_renames.rs:45-98`
    The five `aws_alb*` rules emit a permanent, unsuppressable stylistic WARNING on valid current code; these aliases are still fully supported and not provider-flagged deprecated (contrast aws_s3_bucket_object).
    **Proposal:** Add a `severity` field to `DeprecationRule` and downgrade the alb-alias family to HINT/INFO (keep the auto-fix). Threading the field through ~18 rule literals + 2 emit sites + tests is the bulk.

- [ ] **Shared/reusable modules misclassified as root → false "declared but not used" floods** (medium, effort M, confidence high) — `document.rs:1266`; `module_snapshot.rs:340/322`
    unused_declarations (and typed_variables gating) only fire on root modules, decided by walking indexed `state.documents` for a `module` caller. Opening a standalone shared-module dir (caller outside the workspace) wrongly flags it root, flagging every exposed input as unused.
    **Proposal:** Make root detection tolerant of un-indexed callers — don't flag when no caller is found unless the dir is the workspace root, use lexical path normalisation instead of canonicalize, or gate behind a config opt-in for non-workspace-root dirs.

- [x] **Deprecation and unused-declaration diagnostics never set DiagnosticTag** (medium, effort S, confidence high) — `deprecation_rule.rs:187-193,249-255`; `unused_declarations.rs:134`; schema_validation deprecated paths
    No diagnostic sets `tags`, so deprecations get a plain squiggle instead of strike-through and unused decls aren't greyed out — free standard UX thrown away.
    **Proposal:** Add `DiagnosticTag::DEPRECATED` to deprecation emitters (diagnostics_from_table/from_rule + schema-driven deprecated paths) and `DiagnosticTag::UNNECESSARY` to unused_declarations/unused_required_providers.

- [ ] **did_change has no debounce or in-flight coalescing — every keystroke runs full compute + recomputes all open peers** (medium, effort M, confidence high) — `document.rs:162-229`; `config.rs:51`
    Each keystroke reparses, computes diagnostics for the active doc, then runs full-module `compute_diagnostics` for every other open buffer (O(K × N²) module compute), with no version-staleness guard. `watch_debounce` is watcher-only.
    **Proposal:** Per-uri debounce (dedicated config value, cancel pending compute on newer change), version-stamp publishes to drop stale `spawn_blocking` results, defer peer recompute to the debounce trailing edge.

- [ ] **Background ReparseDocument / publish_for_path / publish_for_dir use O(N²) per-call compute instead of a cached ModuleSnapshot** (medium, effort M, confidence high) — `indexer.rs:369,707,762`
    `publish_for_dir` loops every doc under a dir calling the per-call variant — the O(N²) the doc-comment warns against — so after FetchSchemas/LockFileChanged on a large module every file rebuilds the full aggregate.
    **Proposal:** Group uris by parent dir, build one ModuleSnapshot per dir (as scan_files_parallel does), call `compute_diagnostics_with_lookup`. Preserve canonical/submodule dir-matching.

- [x] **deprecated_index, empty_list_equality, map_duplicate_keys have no paired code action** (index + empty-list done; map-key fix remains) (low, effort M, confidence high) — `document.rs:468-542`
    deprecated_lookup/interpolation have quick-fixes; these three are diagnostic-only despite mechanical fixes (`.N`→`[N]`, `x==[]`→`length(x)==0`, remove overridden duplicate key).
    **Proposal:** Add scan_X functions + emit_scoped_actions wiring. Legacy-index and length() are pure text rewrites; the duplicate-key fix needs new entry-span computation (the diagnostic records only key-name spans).

- [x] **Final dedup stringifies severity via Debug formatting in the hot loop** (low, effort S, confidence high) — `document.rs:601-630`
    The per-call dedup allocates per diagnostic: `format!("{s:?}")` on a Copy enum plus source/message clones into the FxHashSet key, on every compute call.
    **Proposal:** Key on the Copy `DiagnosticSeverity` directly and borrow `&str` for source/message in the retain closure — allocation-free.

- [ ] **publish_peer_diagnostics recomputes peers even when the active edit can't affect cross-file state** (low, effort M, confidence high) — `document.rs:228,245,297-361`
    Every did_change/did_save unconditionally recomputes all open peers, even for value/comment/indentation edits, multiplying per-keystroke cost by open-peer count for no benefit.
    **Proposal:** Capture the active doc's def+ref symbol set (via collect_doc_keys) + a terraform/required_providers fingerprint before vs after apply_change; skip the peer pass when unchanged. The set must include required_version, required_providers, resource type prefixes — not just var/local/module/output decls.

- [ ] **Tier-2 schema-deprecation message gives no replacement; attribute path lacks is_hardcoded guard** (low, effort S, confidence high) — `schema_validation.rs:168-185, 228-240`
    Block/attribute-level tier-2 deprecation diagnostics emit a fixed generic string and discard the schema's own `description`; the attribute path skips the `is_hardcoded_deprecation` guard the block path has.
    **Proposal:** Append the schema description (when present and distinct), add `DiagnosticTag::DEPRECATED`, add the is_hardcoded guard to the attribute path for parity. (`description` is general docs prose, not a migration string — guard for noise.)

- [x] **Unknown-attribute check never flags assigning a computed-only (read-only) attribute** (low, effort S, confidence high) — `schema_validation.rs:241-263`
    The `Some(attr)` arm only checks `attr.deprecated`; assigning a `computed && !optional && !required` attribute (read-only) is silently accepted.
    **Proposal:** In the Some(attr) arm, when `attr.computed && !attr.optional && !attr.required`, emit ERROR "attribute `X` is read-only (computed) and cannot be set". Guard against computed+optional.

- [ ] **Rule message claims 'schema is identical' for Kubernetes renames, but it is not** (low, effort S, confidence high) — `deprecated_kubernetes_renames.rs:68-71`
    The shared message says "schema is identical", contradicting the codebase's own notes that some `_v1` variants (notably HPA) narrow the schema — encouraging a blind label swap that can drop attributes.
    **Proposal:** Reword to warn that some `_v1` variants narrow the schema (call out HPA) and to `terraform plan` before applying; drop the unconditional "schema is identical" claim.

- [ ] **AWS rename family module doc understates current auto-fix coverage** (low, effort S, confidence high) — `deprecated_aws_renames.rs:30-35` + `code_action_block_rename.rs`
    The doc says "All rules are diagnostic-only at present", but the block-rename framework now covers the alb-aliased family and aws_s3_bucket_object/objects.
    **Proposal:** Update the module doc to reflect block-rename auto-fix coverage; note aws_kinesis_analytics_application emits no real `moved` block.

- [x] **map_duplicate_keys misses colon/JSON-style object entries** (low, effort S, confidence high) — `map_duplicate_keys.rs:66-201`
    The hand-rolled tokenizer recognizes a key only before `=`; HCL also accepts `:` (`{ a: 1, a: 2 }`), so those duplicates go undetected.
    **Proposal:** Accept `:` in addition to `=` at the two key-terminator checks. Longer term, derive key spans from parsed Object keys to shrink the bespoke-lexer surface.

- [ ] **naming_convention is hardcoded to snake_case with no configurable format/regex** (low, effort M, confidence high) — `naming_convention.rs:103`
    Hardcodes `[a-z][a-z0-9_]*` with no per-block-type override, unlike tflint's configurable format/custom regex. Already opt-in + INFORMATION, so harm is bounded.
    **Proposal:** Add a `naming_convention` config sub-object (default + per-block-type overrides accepting snake_case|mixed_snake_case|{custom regex}). Needs nested config plumbing + a regex dep (regex::Regex isn't Eq, conflicting with Config's derive — store pattern string + lazy-compile).

- [x] **documented_variables / documented_outputs not paired with a code action** (low, effort S, confidence high) — `documented_variables.rs:39`; `documented_outputs.rs:38`
    Both rules nag "has no description" with no quick-fix to insert a `description = ""` stub.
    **Proposal:** Add a shared `make_add_description_for_diag` quick-fix inserting `description = ""` as the first body attribute, reusing insertion_position; surface via the diagnostic-attached lightbulb path.
