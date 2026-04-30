# CLAUDE.md

## Project Overview

terraform-ls-rs is a high-performance Rust implementation of the Terraform Language Server. Nine-crate Cargo workspace using tower-lsp, hcl-edit, dashmap, sonic-rs, and tokio.

## Common Commands

```bash
# All commands require protoc — use the nix dev shell
nix develop

cargo build --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
cargo bench

# Build release binary
cargo build --release -p tfls-cli

# Run a specific example
cargo run --example probe -- /path/to/.terraform aws_instance ami
```

## Workspace Lints

Strict clippy enforcement — `unwrap_used`, `expect_used`, `panic`, `dbg_macro` are all `deny`. Only tests use `#[allow(...)]` to bypass.

## Architecture

```
crates/
  tfls-core/               Domain types (Symbol, ProviderAddress, ...)
  tfls-parser/             hcl-edit wrapper, position mapping, symbol + ref extraction
  tfls-schema/             Provider schema types, async CLI fetcher, bundled snapshot
  tfls-state/              StateStore (DashMap), DocumentState (rope + AST), JobQueue
  tfls-diag/               Syntax, undefined-ref, schema-validation diagnostics
  tfls-format/             Formatter — thin wrapper around `tf-format`; style runtime-toggleable (see "Formatting style" below)
  tfls-walker/             FS discovery + notify-debouncer-full file watcher
  tfls-provider-protocol/  Terraform plugin gRPC protocol (v5+v6), mTLS, registry docs
  tfls-lsp/                Backend (tower-lsp) + handlers + background indexer
  tfls-cli/                main: tokio, clap, stdio transport
```

Schema fetch has two paths:
1. **Plugin protocol** (primary) — speaks gRPC to provider binaries in `.terraform/providers/`, no credentials needed
2. **CLI fallback** — `tofu providers schema -json` when no `.terraform/providers/` exists

Registry docs enrichment fills missing attribute descriptions (e.g. AWS SDKv2 providers) from the Terraform Registry HTTP API, cached to `$XDG_CACHE_HOME/terraform-ls-rs/provider-docs/`.

## Debug binaries

Three standalone binaries in `crates/tfls-cli/src/bin/` for offline analysis without an LSP client. All use the same `tfls_state::StateStore` + `tfls_lsp::indexer` plumbing as the main `tfls` server, so behaviour matches what a live `did_open` would produce.

### `tfls-diag-dump`

Loads a directory, fetches schemas, runs the full `compute_diagnostics` pipeline over every `.tf` / `.tf.json`, prints results grouped by file. Mirror of what `did_open` would publish.

```bash
cargo run --bin tfls-diag-dump -- <workspace_dir>
cargo run --bin tfls-diag-dump -- <workspace_dir> --errors-only --grep 'undefined'
cargo run --bin tfls-diag-dump -- <workspace_dir> --no-schemas    # skip provider schema fetch
```

Use this first when a user reports "diagnostics not showing up" or "wrong diagnostics" — output isolates server-side correctness from LSP transport / client rendering.

### `tfls-nav-probe`

Tests goto-definition / hover / references at a specific cursor position without driving an LSP client. Pinpoints navigation regressions.

### `tfls-mux-probe`

Spawns an isolated `lspmux server` (random port, override `XDG_CONFIG_HOME` so it doesn't clash with the user's running daemon), then drives N sequential `lspmux client` subprocesses against the same `tfls` binary — each session simulates one nvim launch. Captures `textDocument/publishDiagnostics` per session, prints a summary table, and flags the multi-client republish bug ("session 1 received diagnostics, subsequent sessions did not") when reproduced.

```bash
cargo run --bin tfls-mux-probe -- \
  --tfls-path target/debug/tfls \
  --lspmux-path "$(which lspmux)" \
  --workspace ~/git/terraform/main \
  --file modules/game_server/launchconf.tf \
  --sessions 3
```

After the per-session diagnostic drain, the probe also fires a `textDocument/codeAction` request at `--cursor-line/--cursor-char` (default 0:0) and reports how many actions came back. Use `--print-actions` to dump every title + kind. The codeAction routing bug — session 1 sees actions, session 2+ sees none — surfaces in the summary's `actions=` column. `--no-code-action` skips this probe if you only care about diagnostics.

Use this when investigating LSP message-routing bugs that span multiple client connections (lspmux dedupe, fanout, late-attach republish, codeAction request/response routing). Daemon stderr is captured to `<tmp>/lspmux.stderr.log` for post-mortem.

### `tfls-deprecation-scrape`

Discovers provider-declared deprecations in an initialised workspace's `.terraform/providers/`. Output formats: markdown report (default), JSON, or Rust-scaffold for a single block (drop into `crates/tfls-diag/src/`).

Used to PRIORITISE which deprecations get a hand-written tier-1 `DeprecationRule` (rich migration message + auto-fix action). Tier 2 catches every provider-marked deprecation automatically; this tool surfaces the candidates worth promoting to tier 1.

```bash
# Markdown report of every block-level deprecation across all installed providers:
cargo run --release --bin tfls-deprecation-scrape -- ~/work/some-tf-workspace

# Single provider:
cargo run --release --bin tfls-deprecation-scrape -- <dir> --provider aws

# Long-tail attribute-level (warning: providers mark dozens per release):
cargo run --release --bin tfls-deprecation-scrape -- <dir> --include-attributes

# Scaffold a tier-1 rule for one resource — emits a draft module + wiring instructions:
cargo run --release --bin tfls-deprecation-scrape -- <dir> --scaffold aws_s3_bucket_object > crates/tfls-diag/src/deprecated_aws_s3_bucket_object.rs

# Pipe into other tools:
cargo run --release --bin tfls-deprecation-scrape -- <dir> --format json | jq '.blocks | map(select(.already_covered | not))'

# Curation shortcut: just show candidates not yet covered by tier-1 (no jq):
cargo run --release --bin tfls-deprecation-scrape -- <dir> --uncovered-only
```

The markdown output groups uncovered candidates by provider, surfaces registry-doc URLs (where migration breadcrumbs typically live), and lists already-covered labels separately so curators don't duplicate work. `is_hardcoded_deprecation` from `tfls-diag` is the source of truth for the covered set.

### `tfls-code-action-profile`

Standalone profile driver for the `code_action` handler. Builds a synthetic in-memory workspace (configurable via positional args), fires N code-action requests against the active doc, prints the average. Used for perf regression hunts without spinning up a real LSP client.

```bash
cargo build --release -p tfls-cli --bin tfls-code-action-profile
./target/release/tfls-code-action-profile 500 200          # 500-block fixture, 200 iters
./target/release/tfls-code-action-profile 100 1000         # smaller fixture, more iters
```

When investigating cumulative latency, set `TFLS_PROFILE_CODE_ACTION=1` (handler does NOT currently honour it; instrument locally as needed). Pair with `samply record -- ./target/release/tfls-code-action-profile 500 200` for a flamegraph (kernel `perf_event_paranoid <= 1` required).

### `tfls-infer-coverage`

Variable-type inference coverage report. Walks the workspace (including `.terraform/modules/*` so external module outputs resolve), runs `rebuild_assigned_variable_types_for_dir` on every dir, classifies each declared variable as one of:

- **match** — declared type agrees with inferred shape.
- **mismatch** — both resolved, disagree (often a real authoring bug).
- **no-decl-inferred** — no `type =`, but inference would suggest one (the `Set variable type` quick-fix targets these).
- **no-decl-no-inf** — neither type nor inferable signal.
- **no-inference** — `type =` declared but no caller / default provides a signal. Usually orphaned modules.

```bash
cargo run --bin tfls-infer-coverage -- <workspace_dir>
cargo run --bin tfls-infer-coverage -- <workspace_dir> --list-gaps          # show every no-inference variable + caller expr kind
cargo run --bin tfls-infer-coverage -- <workspace_dir> --dump-dir modules/X # show staged assigned_variable_types[X]
cargo run --bin tfls-infer-coverage -- <workspace_dir> --no-schemas         # skip schema fetch (slashes coverage)
```

Use this when:
- Investigating "code action doesn't suggest a type" — `--list-gaps` shows the caller expression kind so you know whether the gap is a missing schema, a `var.X` chain, an `each.X` pattern, etc.
- After changes to `parse_value_shape_with_schema` / `merge_observations` / `traversal_attr_type` — the percentage figures in commit messages come from this binary.
- Spot-checking a specific module — `--dump-dir` prints the raw `assigned_variable_types` map for one dir.

## Formatting style

The formatter (`crates/tfls-format`) wraps the [`tf-format`](https://github.com/alisonjenkins/tf-format) crate. Two styles, switchable at runtime:

- `minimal` (default) — `terraform fmt` / `tofu fmt` parity. Alignment + spacing only; source order preserved. Safe to apply to any repo.
- `opinionated` — full tf-format behaviour: alphabetises top-level blocks, hoists meta-arguments, sorts attributes/object keys, expands wide single-line objects, adds trailing commas.

Set via either:

1. `initializationOptions.formatStyle` on the LSP `initialize` request:
   ```json
   { "initializationOptions": { "formatStyle": "opinionated" } }
   ```
2. `workspace/didChangeConfiguration` notification (live toggle, no restart):
   ```json
   { "settings": { "terraform-ls-rs": { "formatStyle": "minimal" } } }
   ```

Storage lives on `tfls_state::Config::format_style`; LSP handlers (`textDocument/formatting`, `rangeFormatting`, `onTypeFormatting`) read the live snapshot per-request via `state.config.snapshot()`. Unknown values keep the previous setting.

## Code-action scopes

Every multi-target code action (unwrap interpolation, convert lookup, set variable types, refine `type = any`, declare undefined variables, move outputs to `outputs.tf`, move variables to `variables.tf`, convert `null_resource` to `terraform_data`, convert `data "template_file"` to `templatefile()`) is offered at multiple scopes via `crates/tfls-lsp/src/handlers/code_action_scope.rs`. Diagnostic-only deprecation rules (`data "template_dir"`, `data "null_data_source"`) plug into the same framework but emit no fix.

| Scope       | Iteration set                                     | LSP `CodeActionKind`                                            |
|-------------|---------------------------------------------------|-----------------------------------------------------------------|
| `Instance`  | The single thing under cursor / on a diagnostic   | `quickfix`                                                      |
| `Selection` | Edits whose range intersects `params.range` (when non-empty) | `quickfix.terraform-ls-rs.<id>.selection`            |
| `File`      | Active doc only                                   | `source.fixAll.terraform-ls-rs.<id>`                            |
| `Module`    | Every doc whose parent dir matches the active doc | `source.fixAll.terraform-ls-rs.<id>.module`                     |
| `Workspace` | Every indexed `.tf` doc (skips `.terraform/`)     | `source.fixAll.terraform-ls-rs.<id>.workspace`                  |

`<id>` is a stable per-action identifier (e.g. `unwrap-interpolation`, `convert-lookup-to-index`). Clients filter via `params.context.only` against these kinds; keep them stable.

### Adding a new scoped action

1. Write a per-doc scan: `fn scan_X(uri, body, rope, …) -> Vec<TextEdit>` — pure, returns one edit per occurrence.
2. Call `emit_scoped_actions(state, &uri, selection, include_workspace, "Title verb …", "item label", "action-id", &mut actions, |doc_uri, doc| { scan_X(...) })` from the `code_action` handler.
3. (Optional) For an `Instance` variant attached to a specific diagnostic, write a `make_X_action(uri, diag, …)` that returns a `quickfix` `CodeAction` and call it inside the per-diagnostic loop.

`emit_scoped_actions` handles Selection-range filtering, empty-edit suppression, and title formatting. Unwrap, lookup, set-variable-types, and refine-any all fit this mold; declare-undefined-variables uses a custom helper because module scope needs the union of declarations across sibling files (see `emit_declare_undefined_actions`).

Title format produced by `scope_title` (`"<verb> N <item-label>s in <where>"`):
- `Instance`: title template verbatim, e.g. `"Unwrap interpolation"`.
- `Selection`: `"Unwrap 3 deprecated interpolations in selection"`.
- `File`: `"Unwrap 5 deprecated interpolations in this file"`.
- `Module`: `"Unwrap 12 deprecated interpolations in this module"`.
- `Workspace`: `"Unwrap 47 deprecated interpolations in workspace"`.

## Deprecation framework

`crates/tfls-diag/src/deprecation_rule.rs` holds shared scaffolding for "X is deprecated in Terraform N.M, prefer Y" diagnostics. A deprecation rule is a `const DeprecationRule { block_kind, label, threshold, message }` — adding a new one is one config entry plus three thin wrapper fns (~25 lines).

Live rules:

| Rule / family                                    | Block kind  | Gate                                            | Action                                          |
|--------------------------------------------------|-------------|-------------------------------------------------|-------------------------------------------------|
| `null_resource`                                  | `resource`  | Terraform `>= 1.4.0`                            | Convert to `terraform_data` (+ moved.tf)        |
| `template_file`                                  | `data`      | Terraform `>= 0.12.0`                           | Convert to `local` calling `templatefile()`     |
| `template_dir`                                   | `data`      | Terraform `>= 0.12.0`                           | Diagnostic only                                 |
| `null_data_source`                               | `data`      | Terraform `>= 0.10.0`                           | Diagnostic only                                 |
| AWS rename family (6 resources)                  | `resource`  | AWS provider `>= 1.7.0` / `>= 4.0.0` (s3 object)| **Auto-fix** via generic block-rename action    |
| Kubernetes `_v1` rename family (20 resources)    | `resource`  | kubernetes provider `>= 2.0.0`                  | **Auto-fix** via generic block-rename action    |
| Azure VM split family (2 resources)              | `resource`  | azurerm `>= 2.40.0`                             | Diagnostic only (table)                         |
| GCP Dataflow split                               | `resource`  | google `>= 3.45.0`                              | Diagnostic only (table)                         |
| Vault `vault_generic_secret`                     | `resource`  | vault `>= 3.0.0`                                | Diagnostic only (KV-version-dependent target)   |

Each provider family lives in its own table module
(`crates/tfls-diag/src/deprecated_<provider>_*.rs`). Adding a
new rule to a family = one table entry + one
`HARDCODED_DEPRECATION_LABELS` entry, no new module.

The multi-rule body walker (`deprecation_rule::diagnostics_from_table`) visits each block ONCE regardless of rule count — `(block_kind, label)` HashMap lookup per block, single body iteration. So a table with N entries pays O(blocks) total, not O(blocks × rules). Per-rule gate evaluation runs through the caller's `rule_supported` closure, which the LSP layer wires via `provider_rule_filter(constraint)` (one provider-version constraint extracted per module per code-action call, regardless of how many rules in the table use that provider).

`deprecation_rule::body_supports_rule(rule, body)` is the body-only fallback; `module_constraint_for_provider(state, primary_uri, name)` is the LSP-layer module-aware path. Each provider module provides `<provider>_diagnostics` (body-only convenience) + `<provider>_diagnostics_for_module` (closure-driven, used by `compute_diagnostics_with_lookup`).

### Generic block-rename code action

`crates/tfls-lsp/src/handlers/code_action_block_rename.rs` drives the auto-fix for the AWS and Kubernetes rename families off a single shared `BlockRenameSpec` table. Mechanics per match:

1. **Block label rewrite** — `"<from>"` → `"<to>"` on the matching `<block_kind> "<from>" "X"` block.
2. **Reference rewrite** — every `<from>.X[.attr]` traversal in the body gets its head ident swapped for `<to>` (schemas are identical between the two types, so attribute paths stay the same).
3. **`moved` block emit** — per-spec safety classification (`StateMigration` enum on `BlockRenameSpec`) with three behaviours:
   - `Aliased` (AWS alb family): real `moved {}` blocks emitted unconditionally. `<from>` and `<to>` register the same resource in provider source so state addresses are interchangeable.
   - `RequiresTerraform18` (`aws_s3_bucket_object` → `aws_s3_object`): real `moved {}` emitted ONLY when module's `required_version` admits Terraform 1.8+. Otherwise, **commented-out** `moved` scaffolding emitted with a "REQUIRES TERRAFORM 1.8+" header pointing at either bumping `required_version` or running `terraform state mv` manually.
   - `Manual` (Kubernetes `_v1` family): **commented-out** `moved` scaffolding emitted with a "VERIFY BEFORE UNCOMMENTING" header explaining the user must `terraform plan` first, and giving the `terraform state mv` / `terraform state rm` + `terraform import` paths if `plan` shows destructive changes.

   The commented form gives users the exact `moved {}` syntax pre-written — they uncomment after verification, or follow the alternative-migration breadcrumbs. Beats silently leaving them to author it from scratch.

   Idempotency:
   - Real `moved {}` blocks: HCL-parse existing `moved` blocks across the module, skip names already covered.
   - Commented `moved {}` blocks: text-search existing `moved.tf` for `from = <type>.<name>` substring, skip duplicates.

Multi-scope (Selection / File / Module / Workspace), `CodeActionKind` family `source.fixAll.terraform-ls-rs.rename-deprecated-provider-types[.<scope>]`. Per-call cache keyed by `(module_dir, provider_name)` so a 26-spec table touching 2 providers does at most 2 sibling walks per module per code-action call.

`null_resource → terraform_data` keeps its bespoke action (it has additional attribute-key renames `triggers → triggers_replace` that the generic rename doesn't model). Future consolidation possible if more attribute-rename cases arrive.

### Action surfacing variants

Every block-rename rule surfaces three ways:

1. **Multi-scope source-fixAll** — `source.fixAll.terraform-ls-rs.rename-deprecated-provider-types[.scope]`. Picked up by editor source-action menus / save hooks. Selection / File / Module / Workspace.
2. **Cursor-Instance** — `make_replace_block_at_cursor(state, uri, cursor, body, rope)`. Surfaces a single-block `Convert <from>.<name> to <to>` quickfix when the cursor sits inside a deprecated block. Name-filtered ref rewrites (other instances of the same `<from>` type stay untouched).
3. **Diagnostic-attached lightbulb** — `make_replace_block_for_diag(state, uri, diag, body, rope)`. Wired into the per-diagnostic dispatch loop in `code_action()`. Reuses the cursor-variant block lookup with `diag.range.start` as the cursor; carries the originating diagnostic in the action's `diagnostics` field so the LSP client pairs them.

null_resource and template_file actions also support all three surfacings via their own bespoke `make_X_at_cursor` / `make_X_for_diag` pairs.

Per-table-module test invariants:
- `rule_table_invariants` — every rule has a non-empty message, correct provider, valid block_kind.
- `every_*_is_hardcoded_listed` — every label appears in `HARDCODED_DEPRECATION_LABELS` so the schema-driven tier-2 path doesn't double-fire.

Two gate flavours, set on the rule's `gate: Gate` field:

- **`Gate::TerraformVersion { threshold }`** — checked against `terraform { required_version = "..." }` aggregated across every sibling in the module dir.
- **`Gate::ProviderVersion { provider, threshold }`** — checked against `terraform { required_providers { <provider> = ... } }`. Both short form (`aws = "~> 4.0"`) and long form (`aws = { source = "...", version = "~> 4.0" }`) are recognised.

Module-aware gates live in `crates/tfls-lsp/src/handlers/util.rs`:
- `module_supports_terraform_data`, `module_supports_templatefile`, `module_supports_locals_replacement` — terraform-version gates (`module_constraint_admits_at_least` helper).
- `module_supports_aws_lb` — provider-version gate (`module_provider_constraint_admits_at_least` helper).

Each aggregates the relevant constraint string across every sibling `.tf` in the module dir before deciding. A `terraform { required_version = "..." }` block typically lives in `versions.tf`, not the file the user is editing; per-file gates would miss this.

### Tier 2: schema-driven deprecation warnings

`crates/tfls-diag/src/schema_validation.rs::resource_diagnostics` reads `BlockSchema.deprecated` (set by the provider in its plugin schema) and emits a generic WARNING on the type-name label of any resource / data source the provider has flagged. Attribute-level `AttributeSchema.deprecated` was already wired (line ~87 of that file).

Suppression: when `is_hardcoded_deprecation(block_kind, label)` returns true, the schema-driven warning is skipped — the hardcoded rule provides a richer message + (often) a paired code action. Single source of truth: `HARDCODED_DEPRECATION_LABELS` in `deprecation_rule.rs`.

Why this matters: every provider release adds new deprecations. The hardcoded rules cover the major migrations (those with auto-fix actions); the schema-driven path catches the long tail (~hundreds of attribute renames + a dozen+ resource renames per provider per major release) with zero maintenance burden — the provider's own schema is the source of truth.

### Combined deprecation walker

Reference rewriting (e.g. `null_resource.X.triggers` → `terraform_data.X.triggers_replace`) used to walk the body once per deprecation kind. Now `walk_combined_deprecation_refs` walks each body once and emits flat `RefHit { name: Arc<str>, edit }` rows for every deprecation pattern. Per-call cache `HashMap<Url, CombinedDeprecationRefs>` threads through the scoped emit fns; first emit fn populates a doc, subsequent emits read from cache.

Adding a third (or fourth, or Nth) ref-rewrite deprecation = one new `push_X_hits` leaf check inside the combined walker, NOT another full body walk. Avoids N×walk scaling as the rule set grows.

## Performance caches

Code-action handler runs many independent body scans / formats per invocation. Several caches keep cumulative cost flat as workspaces grow:

| Cache                                     | Scope                  | Invalidation                                    |
|-------------------------------------------|------------------------|-------------------------------------------------|
| `DocumentState::format_cache` (Mutex)     | Cross-call (per-doc)   | `apply_change` clears slot; key `(version, FormatStyle::marker)` |
| Per-call format scan cache                | Single `code_action()` | Drops on return                                  |
| Per-call deprecation scan caches          | Single `code_action()` | Drops on return                                  |
| Combined deprecation ref cache            | Single `code_action()` | Drops on return                                  |
| Module-supports gate cache                | Single emit fn         | Drops on return                                  |

Bench delta from baseline (`tfls-lsp/benches/handlers.rs::code_action_deprecation` at the 500-block synthetic worst-case): **70 ms → ~10.5 ms (-85%)**. Subsequent micro-optimisations: (a) `scan_null_resource_block_edits` + `null_resource_names_in_body` consolidated into one body walk; (b) `scan_blocks_of_kind` swapped its per-byte `rope.byte_slice(end..end+1).to_string()` trailing-whitespace probe for a `rope.to_string()` byte-array indexed scan, halving cost on the move-outputs path. Steady-state breakdown (10ms): `null_resource` 3.5ms, `template_file` 3.2ms, `move_outputs` 2.0ms, everything else <0.5ms total. Real-world workspaces benefit further from the cross-call format cache — repeated code-action menu opens on an unchanged doc skip the formatter entirely.

Bench coverage for the freshly-added block-rename path: `code_action_block_rename` (multi-scope) + `code_action_block_rename_cursor` exercise AWS alb (Aliased) + Kubernetes pod (Manual) at 10 / 100 / 250-500 block scales. All sub-5ms baseline.

### Hashing

All internal per-call caches use `rustc_hash::{FxHashMap, FxHashSet}` — server-internal cache keys (Url / `&'static str` / PathBuf / String) are never untrusted user input, so the std-collection default SipHash 1-3 brings DOS resistance we don't need at the cost of ~2-3× slower lookups on short keys. The `WorkspaceEdit::changes` LSP-types-fixed field stays std `HashMap` — internal accumulators that flow into it convert at the LSP boundary via `into_iter().collect()`. `tfls-state::StateStore`'s eight DashMap/DashSet fields (`documents`, `definitions_by_name`, `references_by_name`, `schemas`, `functions`, `dir_scans`, `fetched_schema_dirs`, `open_docs`, `assigned_variable_types`) all use `FxBuildHasher` via the `FxDashMap` / `FxDashSet` aliases in `tfls-state::store`. `document_link::find_provider_address` is generic over the hasher so the test path (default-hashed map) still typechecks. Bench delta on `code_action_deprecation/large/500_blocks_5_refs`: -2.2% (within noise on smaller variants — code-action latency at this scale is dominated by per-block format scanning, not DashMap lookups, so the Fx win surfaces only on the largest workload).
