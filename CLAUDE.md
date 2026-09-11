# CLAUDE.md

## Project Overview

terraform-ls-rs is a high-performance Rust implementation of the Terraform Language Server. Eleven-crate Cargo workspace using tower-lsp, hcl-edit, dashmap, sonic-rs, and tokio. User-facing documentation (install, editor setup, diagnostics, configuration, `tfls-lint`) lives in [README.md](README.md); this file covers architecture, internals, and the debug tooling.

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

# Lint a workspace in CI
cargo run --bin tfls-lint -- <workspace_dir>

# Run a specific example (package is required — probe.rs lives in tfls-provider-protocol)
cargo run -p tfls-provider-protocol --example probe -- /path/to/workspace aws_instance ami
```

Past investigation and design notes are indexed in [docs/README.md](docs/README.md) — historical record, not living reference.

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
  tfls-engine/             Transport-free diagnostics engine shared by tfls-lsp and tfls-lint. Modules: config_file, format_scan, index, module, pipeline, prefetch, provider_fn, snapshot, workspace
  tfls-lsp/                Backend (tower-lsp) + handlers + background indexer
  tfls-cli/                main: tokio, clap, stdio transport
```

Schema fetch has two paths:
1. **Plugin protocol** (primary) — speaks gRPC to provider binaries in `.terraform/providers/`, no credentials needed
2. **CLI fallback** — `tofu providers schema -json` when no `.terraform/providers/` exists

Registry docs enrichment fills missing attribute descriptions (e.g. AWS SDKv2 providers) from the Terraform Registry HTTP API, cached to `$XDG_CACHE_HOME/terraform-ls-rs/provider-docs/`.

## `tfls-lint`

CI-facing linter, not a debug tool — the standalone binary users are expected to drop into a pipeline. Thin wrapper over `tfls_engine::workspace::{load, lint_all}`: loads one or more workspace roots independently (no cross-root aggregation), runs the same diagnostics pipeline `did_open` would, prints text to stdout, and exits non-zero when findings meet a configurable severity threshold.

```bash
tfls-lint [PATHS...]                    # default: ["."], one root per path
tfls-lint --schemas <plugins|bundled|none>   # default plugins
tfls-lint --fail-on <error|warning|info|hint|never>  # default error
tfls-lint --rule <CODE=off|hint|info|warning|error>  # repeatable
tfls-lint --style-rules                 # opt-in tflint-style rule pack
tfls-lint --jobs <N>                    # rayon worker threads
tfls-lint -q / --quiet                  # summary line only
tfls-lint -v / --verbose                # -v/-vv logging; also enables the schema-outcome '#' lines
tfls-lint --relative-to <DIR>           # print paths relative to DIR instead of each root
tfls-lint --format <text|json|sarif|github>  # default text
tfls-lint --config <FILE>               # explicit .tfls.json instead of per-root discovery
tfls-lint --no-config                   # skip .tfls.json discovery entirely
tfls-lint --offline                     # skip cache warming (see "Cache-backed rules in CI" below)
```

Output: one line per diagnostic to stdout, sorted by `(path, line, col, code)`:
`<relative path>:<line+1>:<col+1>: <severity> [<code>] <message>`. A summary line goes to stderr: `N error(s), N warning(s), N info, N hint(s) in F file(s)`. Schema-fetch failures always print a `warning: schema fetch failed for <root>: <msg>` line to stderr; other schema-outcome detail only appears with `-v`.

Exit codes: `0` clean or below `--fail-on` threshold, `1` findings at/above threshold, `2` tool error (bad path, load failure — printed as `error: ...` with the source chain).

### `--format`: machine-readable output

Rendering lives in `crates/tfls-cli/src/lint_output.rs` (exposed by the `tfls_cli` lib crate so it's unit-testable without spawning the binary) as four pure `fn render_<fmt>(entries: &[(String, Diagnostic)], summary: &Summary) -> String` functions. `main` picks one by `--format` and writes it to stdout; the stderr summary line described above is `text`-only — the machine formats leave stderr quiet (aside from real warnings/errors, e.g. schema-fetch failures). Positions are 1-based in every format, matching `text`.

- **`text`** (default) — the format above.
- **`json`** — stable document, `serde_json`-derived:
  ```json
  {
    "version": 1,
    "summary": { "errors": 0, "warnings": 3, "info": 0, "hints": 0, "files": 1 },
    "diagnostics": [
      { "path": "main.tf", "line": 5, "column": 1, "end_line": 5, "end_column": 9,
        "severity": "warning", "code": "terraform_unused_declarations",
        "message": "variable `unused` is declared but not used",
        "source": "terraform-ls-rs" }
    ]
  }
  ```
- **`sarif`** — SARIF 2.1.0, one run, `tool.driver.rules` populated with one rule per distinct code (sorted); `level` maps `error`→`error`, `warning`→`warning`, `info`/`hint`→`note`; `artifactLocation.uriBaseId` is `%SRCROOT%`. What GitHub code scanning's `upload-sarif` action consumes.
- **`github`** — one [GitHub Actions workflow command](https://docs.github.com/en/actions/using-workflows/workflow-commands-for-github-actions) per finding: `::<error|warning|notice> file=<path>,line=<L>,endLine=<L>,col=<C>,endColumn=<C>,title=<code>::<message>` (severity `info`/`hint` → `notice`). Values are escaped per the Actions spec (`%`→`%25`, `\r`→`%0D`, `\n`→`%0A` in the message; additionally `:`→`%3A`, `,`→`%2C` in property values).

CI examples:

```yaml
- run: tfls-lint --format github --fail-on warning .
```

```yaml
- run: tfls-lint --format sarif . > tfls.sarif
- uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: tfls.sarif
```

`--rule` / `--style-rules` build the same `{"rules": {...}, "styleRules": ...}` JSON shape the LSP `initializationOptions`/`didChangeConfiguration` accept (see "Per-rule diagnostic config" below) and apply it to each loaded root's `state.config` before linting.

### Cache-backed rules in CI

Four rules read on-disk caches under `$XDG_CACHE_HOME/terraform-ls-rs/` instead of fetching over the network inline: `terraform_constraint` and `terraform_lock_constraint_drift` (Terraform/OpenTofu CLI + registry provider/module version catalogues), `terraform_module_outdated` and `terraform_module_ref_tag_mismatch` (git module tag lists, `tfls_provider_protocol::git_refs`). In the LSP these caches are warmed in the background (`crates/tfls-lsp/src/handlers/version_prefetch.rs`); a fresh CI runner has no such background job and no warm cache, so without warming these four rules silently never fire — no error, no signal, just missing findings.

`tfls-lint` warms the cache by default: after loading a root and before `lint_all`, it calls `tfls_engine::prefetch::{collect_warm_targets, warm_caches}` — the same walk-the-`StateStore`-and-fetch core the LSP prefetch is built on (`crates/tfls-engine/src/prefetch.rs`) — over every `required_version` / `required_providers` / module `source` target found in the root. A fetch failure (offline runner, rate limit, DNS) is always a warning on stderr (`warning: cache warm failed for <target>: <err>`) and never changes the exit code; `-v` additionally prints a one-line fetched/cached/failed summary. Pass `--offline` to skip warming entirely (e.g. a runner that intentionally has no network and wants to lint against whatever cache already exists, with no warning noise).

Speed up repeat CI runs by caching `~/.cache/terraform-ls-rs` between them:

```yaml
- uses: actions/cache@v4
  with:
    path: ~/.cache/terraform-ls-rs
    key: tfls-cache-${{ runner.os }}
```

## Debug binaries

`crates/tfls-cli/Cargo.toml` declares 12 `[[bin]]` targets: the main
`tfls` server, `tfls-lint` (documented above), and 10 standalone probes
in `crates/tfls-cli/src/bin/` for offline analysis without an LSP
client. Most share the same `tfls_state::StateStore` + `tfls_lsp::indexer`
plumbing as the main `tfls` server, so behaviour matches what a live
`did_open` would produce; `tfls-lint` is the exception — it goes through
`tfls_engine::index` instead, since it has no LSP session to indexer.

### `tfls-diag-dump`

Loads a directory, fetches schemas, runs the full `compute_diagnostics` pipeline over every `.tf` / `.tf.json`, prints results grouped by file. Mirror of what `did_open` would publish. Thin CLI wrapper over `tfls_engine::workspace::{load, lint_all}`.

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

### `tfls-doc-probe`

Inspects registry-doc enrichment for a single provider. Hover descriptions for SDK-v2 / Plugin-Framework providers come from the Terraform Registry's hand-written Markdown rather than the gRPC schema (most providers ship empty descriptions over the wire). The enrichment pipeline in `tfls-provider-protocol::registry_docs` is best-effort and silently skips providers whose Markdown shape the parser doesn't recognise — this binary surfaces the pipeline state so a "hover doesn't work for X" report is one command away from a root cause.

```bash
# Inspect cache + index, then parse one resource's docs:
cargo run --bin tfls-doc-probe -- hashicorp/azurerm@4.50.0 \
    --resource azurerm_automation_runbook

# Walk every resource in the index and report ones whose parser
# output is empty (canary for unfamiliar Markdown shape):
cargo run --bin tfls-doc-probe -- hashicorp/azurerm@4.50.0 --list-uncovered

# Dump the raw Markdown content alongside parser output:
cargo run --bin tfls-doc-probe -- hashicorp/azurerm@4.50.0 \
    --resource azurerm_automation_runbook --show-markdown

# Force a re-fetch (purges the doc + parsed cache slots):
cargo run --bin tfls-doc-probe -- hashicorp/azurerm@4.50.0 \
    --resource azurerm_automation_runbook --no-cache
```

Use this first when a user reports "hover descriptions empty for X". Output flags whether the parsed-descriptions cache exists, whether the registry index returned non-zero docs, and whether the Markdown parser produced any attribute entries. Each item also shows the mined `[valid: \`X\`, \`Y\`]` enum (when found) so you can audit the `extract_allowed_values` heuristics from real docs.

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

### `tfls-lock-probe`

End-to-end `.terraform.lock.hcl` invalidation probe. Boots the same plumbing the real LSP server uses (`StateStore`, `JobQueue`, `tfls_lsp::indexer::spawn_watcher`) against a temp workspace, rewrites the lock file to simulate a `terraform init`, and reports what `state.lock_file_for(...)` / `compute_diagnostics(...)` see after each step. Catches cache-key / watcher-path / debounce mismatches that unit tests miss but the real `notify-debouncer-full` crate triggers.

```bash
cargo run --bin tfls-lock-probe -- <workspace_dir>
cargo run --bin tfls-lock-probe -- <workspace_dir> --wait-ms 600   # ms between mutating the lock file and querying state; keep above the watcher's debounce window
cargo run --bin tfls-lock-probe -- <workspace_dir> --verbose       # dump every parsed lock entry at each step
```

### `tfls-mux-lock-probe`

Lock-file-change-through-lspmux probe. Boots an isolated `lspmux` daemon plus one `lspmux client` subprocess against a fresh `tfls`, drives `initialize`/`didOpen`, mutates `.terraform.lock.hcl` mid-session, and reports which `textDocument/publishDiagnostics` notifications actually arrive. Pins whether the lock-to-diagnostic refresh chain breaks in `tfls`'s in-process flow or in lspmux's fanout routing.

```bash
cargo run --bin tfls-mux-lock-probe -- \
  --tfls-path target/debug/tfls --lspmux-path "$(which lspmux)" \
  --workspace <workspace_dir> --drain-ms 1500
cargo run --bin tfls-mux-lock-probe -- --direct --workspace <workspace_dir>   # skip lspmux, JSON-RPC straight to tfls's stdio
```

### `tfls-mux-format-probe`

Reproduces a reported bug where diagnostics render on the wrong line after an opinionated reformat reorders blocks: opens a synthetic file ordered so the opinionated formatter reshuffles blocks, drains initial diagnostics, sets `formatStyle=opinionated` via `didChangeConfiguration`, formats, applies the edits, sends `didChange`, then checks post-format diagnostics still point at the right line. The in-process counterpart is `tfls-lsp/tests/phase4.rs::opinionated_format_then_diagnostics_align_to_new_buffer`; this probe drives the real LSP transport (optionally through lspmux) to catch transport-layer routing bugs the in-process test can't see.

```bash
cargo run --bin tfls-mux-format-probe -- \
  --tfls-path target/debug/tfls --lspmux-path "$(which lspmux)" --drain-ms 2500
cargo run --bin tfls-mux-format-probe -- --direct   # skip lspmux, spawn tfls directly
```

## Formatting style

User-facing description (what `minimal`/`opinionated` do, how to set them) is in [README.md](README.md#formatting-two-styles).

The formatter (`crates/tfls-format`) wraps the [`tf-format`](https://github.com/alisonjenkins/tf-format) crate. Storage lives on `tfls_state::Config::format_style`; LSP handlers (`textDocument/formatting`, `rangeFormatting`, `onTypeFormatting`) read the live snapshot per-request via `state.config.snapshot()`. Unknown values keep the previous setting.

### Unformatted-file diagnostic (`terraform_fmt`)

`compute_diagnostics_with_lookup` emits an INFORMATION diagnostic when a buffer isn't formatted to the active `formatStyle` (minimal = `terraform fmt`/`tofu fmt` parity, opinionated = full tf-format). Implemented by `tfls_engine::format_scan::formatting_diagnostic`, which reuses `scan_format_cached` (the per-doc `format_cache`, keyed by `(version, FormatStyle::marker)`) — so an already-formatted, unedited buffer is a no-op, and any edit clears the cache (`apply_change` sets it to `None`) so a change that breaks formatting is picked up on the next compute. `tfls-lsp`'s format code action re-exports `scan_format_cached` from `handlers::code_action` to share the same cache. Ranges at the first differing line; pairs with the existing format code action. A file that doesn't parse yields no fmt diagnostic (the formatter errors; the syntax-error diagnostic covers it). Default-on; disable or retune via the per-rule config (`{"rules": {"terraform_fmt": "off"}}`).

## Per-rule diagnostic config

The full rule-code table and JSON syntax are in [README.md](README.md#diagnostics). Mechanism: each rule's output is tagged with a `terraform_<rule>` code at its `compute_diagnostics_with_lookup` call site (in `crates/tfls-engine/src/pipeline.rs`, via the `tag()` wrapper); a final `apply_rule_overrides` post-pass (before dedup) drops `off` codes and remaps the rest. Storage: `tfls_state::Config::rule_overrides` (`Arc<HashMap<String, RuleSeverity>>`, replaced wholesale per update so dropping a key restores the default). Live-toggle works because `did_change_configuration` already republishes open docs.

Adding a code to a new rule = wrap its call site with `tag("terraform_<id>", …)`. Untagged diagnostics pass through unaffected.

### Project config file (`.tfls.json`)

A repo can check in one `rules`/`styleRules`/`formatStyle` policy that both the editor and CI read, instead of configuring each side separately. `crates/tfls-engine/src/config_file.rs`:

- `find_config_file(start)` looks for `.tfls.json` in `start` and each ancestor directory, stopping at the filesystem root; the nearest ancestor wins. No git-root heuristic.
- `load_config_file(path)` parses the file as the same settings object `initializationOptions`/`didChangeConfiguration` accept — no wrapper key, so the file's content is exactly `{"rules": {...}, "styleRules": true, "formatStyle": "minimal"}`.

**LSP precedence:** project config file → `initializationOptions` → `workspace/didChangeConfiguration`, each later step winning over the earlier ones. Note `rules` is replaced as a whole map at each step (existing `update_from_json` semantics, so dropping a key restores the default): an `initializationOptions` payload that carries any `rules` key discards every rule the file set, rather than overriding key by key. Editors that only want the project policy should omit `rules` from `initializationOptions`. `Backend::initialize` discovers `.tfls.json` for every `workspace_folders`/`root_uri` path (deduplicated) and applies it before `initializationOptions` are applied, so a user's explicit editor settings still take precedence, and a later `didChangeConfiguration` continues to override at runtime as before. A missing file is silent; a present-but-invalid one logs a `warn` with the error chain and never fails `initialize`. **Not implemented**: reloading when `.tfls.json` itself is edited after `initialize` — a follow-up.

**CLI precedence:** project config file → `--rule`/`--style-rules` flags, flags winning. `tfls-lint` defaults to `find_config_file(root)` per root; `--config <FILE>` loads an explicit file instead (applied to every root; exits 2 if unreadable or not a JSON object), and `--no-config` skips discovery entirely. `-v` prints `# config: <path>` to stderr when a file is applied.

### Untagged-resource diagnostics (`terraform_missing_tags`, `terraform_missing_name_tag`)

`crates/tfls-diag/src/missing_tags.rs` emits two default-on WARNING rules for untagged resources:

- `terraform_missing_tags` — schema-driven, provider-agnostic. A `resource` whose schema declares a `tags` (AWS/Azure) or `labels` (GCP/Kubernetes) attribute but sets neither. Requires fetched provider schemas (silent otherwise). Suppressed for any provider that declares `default_tags` — `crates/tfls-engine/src/pipeline.rs` aggregates provider local names across module siblings via `module::module_providers_with_default_tags` (`crates/tfls-engine/src/module.rs:184`, built on `tfls_diag::provider_names_with_default_tags`) and passes them in, since `provider "aws" { default_tags {} }` usually lives in `provider.tf`/`versions.tf`.
- `terraform_missing_name_tag` — AWS-specific, schema-free (works before `.terraform/providers` is fetched). A `resource` of a curated console-visible type (`AWS_NAME_TAG_RESOURCES` table — `aws_instance`, `aws_vpc`, `aws_subnet`, … extend as needed) that lacks a statically-visible literal `Name` tag key. The whole `tags` expression is scanned for an object key `Name` (so `merge(common, { Name = x })` passes; `var.tags` warns). `Name` matters because these types show their `Name` tag in the AWS console.

Both anchor on the type-name label, are off-able / re-severitied via the `rules` config above, and emit no code action (diagnostic only).

## Code-action scopes

Every multi-target code action (unwrap interpolation, convert lookup, set variable types, refine `type = any`, module-shallow-clone-depth, declare undefined variables, move outputs to `outputs.tf`, move variables to `variables.tf`, convert `null_resource` to `terraform_data`, convert `data "template_file"` to `templatefile()`, rename-deprecated-provider-types) is offered at multiple scopes via `crates/tfls-lsp/src/handlers/code_action_scope.rs`. Diagnostic-only deprecation rules (`data "template_dir"`, `data "null_data_source"`, azurerm VM split, GCP Dataflow split, vault) plug into the same framework but emit no fix. See README's [code actions across scopes](README.md#code-actions-across-scopes) for the full `<id>` list and user-facing scope behaviour.

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
| AWS rename family (9 resources/data sources)     | `resource`, `data` | AWS provider `>= 1.7.0` / `>= 4.0.0` (s3 object, s3 objects, kinesis analytics) | **Auto-fix** via generic block-rename action    |
| Kubernetes `_v1` rename family (20 resources)    | `resource`  | kubernetes provider `>= 2.0.0`                  | **Auto-fix** via generic block-rename action    |
| Azure VM split family (2 resources)              | `resource`  | azurerm `>= 2.40.0`                             | Diagnostic only (table)                         |
| GCP Dataflow split                               | `resource`  | google `>= 3.45.0`                              | Diagnostic only (table)                         |
| Vault `vault_generic_secret`                     | `resource`  | vault `>= 3.0.0`                                | Diagnostic only (KV-version-dependent target)   |

Each provider family lives in its own table module
(`crates/tfls-diag/src/deprecated_<provider>_*.rs`). Adding a
new rule to a family = one table entry + one
`HARDCODED_DEPRECATION_LABELS` entry, no new module.

The multi-rule body walker (`deprecation_rule::diagnostics_from_table`) visits each block ONCE regardless of rule count — `(block_kind, label)` HashMap lookup per block, single body iteration. So a table with N entries pays O(blocks) total, not O(blocks × rules). Per-rule gate evaluation runs through the caller's `rule_supported` closure, which `crates/tfls-engine/src/pipeline.rs` wires via `provider_rule_filter(&constraint, locked_version.as_ref())` (one provider-version constraint extracted per module per code-action call, regardless of how many rules in the table use that provider; the second argument is the locked version from `.terraform.lock.hcl` when available).

`deprecation_rule::body_supports_rule(rule, body)` is the body-only fallback; `module_constraint_for_provider(state, primary_uri, name)` (`crates/tfls-engine/src/module.rs:382`) is the engine's module-aware path, shared by `tfls-lsp` and `tfls-lint`. Each provider module provides `<provider>_diagnostics` (body-only convenience) + `<provider>_diagnostics_for_module` (closure-driven, used by `compute_diagnostics_with_lookup`).

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

Multi-scope (Selection / File / Module / Workspace), `CodeActionKind` family `source.fixAll.terraform-ls-rs.rename-deprecated-provider-types[.<scope>]`. Per-call cache keyed by `(module_dir, provider_name)` so the `ALL_BLOCK_RENAMES` table (29 entries: 9 aws, 20 kubernetes) touching 2 providers does at most 2 sibling walks per module per code-action call.

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

Module-aware gates live in `crates/tfls-engine/src/module.rs`:
- `module_supports_terraform_data`, `module_supports_templatefile`, `module_supports_locals_replacement` — terraform-version gates (`module_constraint_admits_at_least` helper).
- Provider-version gates (AWS, Kubernetes, azurerm, google, vault) go through the generic `module_constraint_for_provider` + `provider_rule_filter` path described above, not a per-provider function — there is no `module_supports_aws_lb` any more; the AWS ALB family is just another row in `ALL_BLOCK_RENAMES` gated by the same mechanism as every other provider family.

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

Run `cargo bench -p tfls-lsp --bench handlers -- code_action_deprecation` for current numbers on your hardware; no historical figure is republished here since the last one shipped undated and had already drifted from README's copy of the same claim (see the docs audit that prompted this rewrite). Two micro-optimisations worth knowing about when reading a profile: (a) `scan_null_resource_block_edits` + `null_resource_names_in_body` are consolidated into one body walk; (b) `scan_blocks_of_kind` uses a `rope.to_string()` byte-array indexed scan for its trailing-whitespace probe instead of a per-byte `rope.byte_slice` call, which mattered most on the move-outputs path. Real-world workspaces benefit further from the cross-call format cache — repeated code-action menu opens on an unchanged doc skip the formatter entirely.

Bench coverage for the block-rename path: `code_action_block_rename` (multi-scope) + `code_action_block_rename_cursor` exercise AWS alb (Aliased) + Kubernetes pod (Manual) at 10 / 100 / 250-500 block scales.

### Hashing

All internal per-call caches use `rustc_hash::{FxHashMap, FxHashSet}` — server-internal cache keys (Url / `&'static str` / PathBuf / String) are never untrusted user input, so the std-collection default SipHash 1-3 brings DOS resistance we don't need at the cost of ~2-3× slower lookups on short keys. The `WorkspaceEdit::changes` LSP-types-fixed field stays std `HashMap` — internal accumulators that flow into it convert at the LSP boundary via `into_iter().collect()`. `tfls-state::StateStore` has 13 Fx-hashed collection fields (12 `FxDashMap`, 1 `FxDashSet`) via the `FxDashMap` / `FxDashSet` aliases in `tfls-state::store`:

`documents`, `definitions_by_name`, `references_by_name`, `schemas`, `functions`, `dir_scans`, `fetched_schema_dirs`, `installed_provider_versions`, `open_docs` (the `FxDashSet`), `assigned_variable_types`, `unknown_module_vars`, `locks`, `locks_mtime`.

`document_link::find_provider_address` is generic over the hasher so the test path (default-hashed map) still typechecks. Non-collection fields on `StateStore` (`config`, the pull-diagnostics-capability `AtomicBool`s) don't count toward this figure.
