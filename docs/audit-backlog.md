# Bug Audit Backlog

Source: multi-agent repo audit (per-crate finders → adversarial verify → synthesize), 2026-06-02.
16 confirmed bugs (deduped) — **3 high, 8 medium, 5 low**. All adversarially verified; several empirically reproduced.

Workflow: `audit-repo-bugs`. Each finding read by a skeptic prompted to refute; only survivors listed.

---

## High

- [x] **Deadlock: `code_action` holds `documents` read guard across recursive locks** — `crates/tfls-lsp/src/handlers/code_action.rs:34`
  The shard read guard from `state.documents.get(&uri)` is held until `drop(doc)` at line 244, but pre-drop builders recursively re-lock the same map: `make_unknown_provider_local_quickfixes` (108) → `collect_required_providers_locals` (re-`get`/`iter` 2574/2580); `module_supports_terraform_data` (222) → `iter()` at `util.rs:163`; cursor `make_replace_*` → `collect_existing_moved_names` (2049). parking_lot's write-preferring RwLock makes a concurrent `did_change`/upsert writer on that shard deadlock the recursive read.
  **Fix:** drop the guard (or copy needed body/rope by value/Arc) before the per-diagnostic loop and cursor builders.

- [x] **`for_each_expression` never recurses into heredoc template interpolations** — `crates/tfls-diag/src/expr_walk.rs:50-55`
  `Expression::HeredocTemplate` matched as a leaf alongside Null/Bool/Number/String/Variable, so expressions inside `<<-EOT ${...} EOT` are invisible to all six consumers (deprecated_lookup/index/interpolation, empty_list_equality, map_duplicate_keys, workspace_remote). Systematic false-negative; contradicts the module's "no expression position is missed" contract and the parser's own `references.rs:110` which does recurse. Empirically reproduced (0 diagnostics where 1 expected).
  **Fix:** add `Expression::HeredocTemplate(h) => visit_template(&h.template, visit)`; remove it from the leaf group.

- [x] **`compute_index_replace_range` panics on past-EOL / non-boundary cursor column** — `crates/tfls-lsp/src/handlers/completion.rs:2844`
  `let after = &line[col..]` uses raw unclamped `pos.character` as a byte index. The completion handler clamps only `offset`, forwarding raw `pos` to `index_key_items`; a cursor past EOL (or `pos.character` as UTF-16 landing mid-codepoint) makes `col > line.len()` / non-boundary and panics the request task — while the prefix on line 2841 is already defended with `line.get(..col).unwrap_or("")`. Reachable on `var.x[`-style IndexKeyRef completions.
  **Fix:** `let after = line.get(col..).unwrap_or("");` (ideally also snap `col` to a char boundary).

---

## Medium

- [x] **`scan_body_for_source_attr` matches `source` as a substring of other attribute names** — `crates/tfls-core/src/completion.rs:413`
  Unanchored `rest.find("source")` matches `data_source = "..."`, `config_source = "..."`, returning the wrong value into `RequiredProviderVersionValue.source` / `ModuleVersionValue.source`, mis-scoping registry version completion. Repro: `data_source = "aws_caller_identity"` yields `Some("aws_caller_identity")`.
  **Fix:** require an identifier boundary before `idx` (or tokenize and compare the whole key to `"source"`).

- [x] **`satisfies_one` compares Eq/Ne by raw string instead of semantic version key** — `crates/tfls-core/src/version_constraint.rs:415`
  Gt/Gte/Lt/Lte compare parsed `VersionKey`; Eq/Ne use `candidate == c.version`, so `= 1.2` fails to match `1.2.0`, feeding wrong "latest matching" inlay-hint labels.
  **Fix:** `Eq => candidate_key == &constraint_key`, `Ne => candidate_key != &constraint_key`.

- [x] **JSON object keys emitted unquoted, producing invalid/misparsed HCL** — `crates/tfls-parser/src/json.rs:231`
  `write_value` pushes object keys verbatim (`key = value`); non-identifier `.tf.json` keys (`"eu-west-1"`, `"with space"`, `"123"`) generate un-parseable HCL, so the whole document's body becomes `None` and downstream references/diagnostics see nothing. `escape_string` applied to values but never keys. Reproduced (`body_some=false`, `invalid object value assignment`).
  **Fix:** always emit `"<escape_string(k)>" = ...`.

- [x] **`fetch_provider_functions` omits `max_decoding_message_size` — functions never load for large providers** — `crates/tfls-provider-protocol/src/client.rs:158`
  Constructs `ProviderClientV6::new(channel)` without the 256 MiB cap that `fetch_schema_v6`/`v5` apply, while issuing the same full-schema `get_provider_schema` RPC; AWS/azurerm/google responses exceed tonic's 4 MiB default and fail, and the caller (`indexer.rs:1690`) only logs at debug. Provider-defined functions silently never install for the largest providers.
  **Fix:** add `.max_decoding_message_size(256 * 1024 * 1024)`.

- [x] **Secondary-index maintenance not atomic across concurrent writers** — `crates/tfls-state/src/store.rs:563`
  `upsert_document`/`reparse_document`/`remove_document` maintain the `documents` ↔ `definitions_by_name`/`references_by_name` invariant via independent DashMap ops (only per-shard atomic). Invoked concurrently from the worker task, the watcher (`indexer.rs:271`), and `did_change`/`did_save` on `spawn_blocking` — interleaving on the same URI can leave orphaned index locations or partially-cleared indexes, corrupting goto-definition/find-references and undefined-ref diagnostics.
  **Fix:** guard the three mutators with one coarse `parking_lot::Mutex<()>`, or route all mutations through the single worker task.

- [x] **Symlinked directories and `.tf` files silently skipped by all discovery walkers** — `crates/tfls-walker/src/discovery.rs:41`
  `DirEntry::file_type()` doesn't follow symlinks, so for a symlink both `is_dir()` and `is_file()` are false; every discovery fn branches only on those, dropping symlinked shared modules / tfvars (common Terragrunt/monorepo layout) from indexing.
  **Fix:** on `is_symlink()`, resolve via `fs::metadata(&path)` and branch off that, tracking canonicalized dirs in a `HashSet` to guard cycles.

- [x] **Lock-file poller emits `LockFileChanged` for every pre-existing lock file on first scan** — `crates/tfls-walker/src/watcher.rs:155`
  `seen` starts empty, so the first scan classifies every existing `.terraform.lock.hcl` as `None => true` and emits a change; the indexer handler (`indexer.rs:274`) then `invalidate_lock` + re-enqueues an expensive schema fetch per module — a re-fetch storm on every startup of an already-initialised workspace.
  **Fix:** prime `seen` with one initial scan before the loop (or suppress emits on the first pass).

- [x] **`deprecation-scrape` emits wrong registry-doc URLs for non-hashicorp providers** — `crates/tfls-cli/src/bin/deprecation_scrape.rs:321`
  `registry_url` hardcodes the `hashicorp` namespace; `local_provider_name` keeps only the final path segment (name), discarding the namespace, so third-party providers (`integrations/github`, `cloudflare/cloudflare`, `oracle/oci`) get 404 `hashicorp/<name>` links — defeating the tool's breadcrumb purpose.
  **Fix:** parse the `host/namespace/name` key, carry the namespace on `DepBlock`/`DepAttribute`, build `.../providers/{namespace}/{name}/...`.

---

## Low

- [x] **`required_providers_version_diagnostics` invoked twice for the same document** — `crates/tfls-lsp/src/handlers/document.rs:567`
  Two byte-identical back-to-back calls; defensive dedup masks duplicate output but doubles pass cost, and would fail to collapse same-range/different-message entries.
  **Fix:** delete the duplicate call at lines 570-572.

- [x] **`exactly_one_of` / `conflicts_with` emit duplicate diagnostics for a symmetric violation** — `crates/tfls-diag/src/schema_validation.rs:338`
  Loop scans every present attribute and reports symmetric group violations from both ends → one logical conflict yields two squiggles. Adjacent `at_least_one_of` block already dedupes via `seen_groups`; this path does not.
  **Fix:** dedupe unordered pairs (emit only when `name < other`, or track reported pairs in a set).

- [x] **`version_key` silently truncates versions with >3 numeric segments** — `crates/tfls-core/src/version_constraint.rs:468`
  `splitn(3, '.')` makes `1.2.3.4` parse its patch from `"3.4"`, failing to `0`, so `1.2.3.4` keys as `1.2.0`. `validate_version` admits 4-segment strings, so they reach comparison.
  **Fix:** split the patch from only the third segment, or collect all numeric segments and compare lexicographically.

- [x] **Symbol fallback ranges use Unicode-scalar columns, diverging from byte-offset position contract** — `crates/tfls-parser/src/fallback_symbols.rs:363`
  `byte_to_lsp` returns `char_idx - line_start_char` (scalar count) while the crate-wide `position.rs` contract (and sibling `fallback_references.rs`) uses byte differences; in text-fallback mode, multibyte chars before a label shift goto-definition/rename/document-symbol columns.
  **Fix:** reuse `crate::position::byte_offset_to_lsp_position`.

- [x] **`scan_locals_body` ignores block comments, corrupting depth tracking** — `crates/tfls-parser/src/fallback_symbols.rs:262`
  No `in_block_comment` state, so a `}` inside `/* */` in a locals body decrements `depth` and can terminate the scan early, dropping locals after the comment (`locals { /* } */\n real = 1 }` loses `real`). Fallback-only path.
  **Fix:** add `in_block_comment` handling mirroring the outer scanner.

- [x] **`fallback_references` scans inside nested string literals within interpolations** — `crates/tfls-parser/src/fallback_references.rs:117`
  Entering an interpolation never clears `in_string`, so the `b'"' if !in_string` arm can't re-enter string mode; quoted text inside `${...}` (e.g. `"${lookup(m, "var.x")}"`) is mined as code, yielding phantom references. Fallback-only.
  **Fix:** track a separate nested-string state while `interp_depth > 0`.
