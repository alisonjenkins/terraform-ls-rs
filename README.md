# terraform-ls-rs

A fast Rust implementation of the Terraform / OpenTofu language server,
built to fix the latency and memory problems of HashiCorp's Go-based
`terraform-ls`.

`terraform-ls` regularly uses 2-10 GB of RAM on a moderately sized
workspace, pegs a CPU core for minutes during indexing, and can leave
stale errors on screen after the underlying code is fixed. The causes:
Go's GC pressure, synchronous `terraform` CLI calls, full re-parses on
every edit, and `go-memdb` overhead.

This project replaces those pieces with:

- [`hcl-edit`](https://docs.rs/hcl-edit) for HCL parsing with preserved
  position info
- [`ropey`](https://docs.rs/ropey) for O(log n) incremental edits
- [`dashmap`](https://docs.rs/dashmap) for lock-free concurrent state
- [`sonic-rs`](https://docs.rs/sonic-rs) for SIMD-accelerated JSON parsing
  of provider schemas
- [`tokio`](https://tokio.rs) async everywhere, so CLI schema fetches
  never block the server thread
- [`tower-lsp`](https://docs.rs/tower-lsp) for the LSP protocol

The same diagnostics engine ships three ways: an LSP server (`tfls`), a
CI linter (`tfls-lint`), and a GitHub Action wrapping the linter. All
three run identical checks, so a warning you silence in your editor
stays silenced in CI when you check in a `.tfls.json`.

## Install

### Using Nix (recommended)

```sh
# Run it once without installing
nix run github:alisonjenkins/terraform-ls-rs

# Install into your profile
nix profile install github:alisonjenkins/terraform-ls-rs

# Drop into a dev shell with fenix-managed Rust + OpenTofu + rust-analyzer
nix develop
```

The flake exposes `packages.default` / `packages.tfls` (the server),
`apps.default` (runs `tfls`), and `apps.tfls-lint` (runs the CI linter):

```sh
nix run github:alisonjenkins/terraform-ls-rs#tfls-lint -- .
```

### Using Cargo

```sh
cargo install --git https://github.com/alisonjenkins/terraform-ls-rs --bin tfls --bin tfls-lint
```

`--bin tfls --bin tfls-lint` installs only the server and the linter.
Without it, Cargo installs every binary in the crate, including a dozen
debug probes meant for local investigation, not day-to-day use (see
[CLAUDE.md](CLAUDE.md) if you want those too). The crate isn't published
to crates.io, so `--git` is required.

### Prebuilt binaries

Each [GitHub release](https://github.com/alisonjenkins/terraform-ls-rs/releases)
ships `tar.gz` archives for two platforms:

- `tfls-<version>-x86_64-unknown-linux-musl.tar.gz`
- `tfls-<version>-x86_64-pc-windows-msvc.tar.gz`
- `tfls-lint-<version>-x86_64-unknown-linux-musl.tar.gz`
- `tfls-lint-<version>-x86_64-pc-windows-msvc.tar.gz`

Each archive has a matching `.sha256` file. There is no macOS build.
macOS users build from source through `nix develop` or `cargo install`
above.

## Editor setup

### Neovim

`nvim-lspconfig` has no built-in preset for `tfls`. Start it directly
with `vim.lsp.start`, which works on any Neovim 0.10+ without depending
on lspconfig's internal API:

```lua
vim.api.nvim_create_autocmd('FileType', {
  pattern = { 'terraform', 'terraform-vars' },
  callback = function()
    vim.lsp.start({
      name = 'tfls',
      cmd = { 'tfls' },
      root_dir = vim.fs.root(0, { '.terraform', '.git' }),
    })
  end,
})
```

### VS Code

A dedicated extension lives in [`editors/vscode`](editors/vscode). It is
in **preview** and not yet on the Marketplace. Install the `.vsix`
attached to a [GitHub release](https://github.com/alisonjenkins/terraform-ls-rs/releases)
(`code --install-extension tfls-vscode-<version>.vsix`). On first
activation it downloads the matching `tfls` binary for your platform
(Linux x64 or Windows x64 — no macOS build), verifies its checksum, and
caches it; set `terraform-ls-rs.serverPath` to use a local build instead.

## Features

Everything `hashicorp/terraform-ls` supports, plus rename, document
highlight, folding, inlay hints, on-type formatting, semantic tokens,
and pull diagnostics. The full method list lives in
`crates/tfls-lsp/src/backend.rs`; grouped by capability:

| Capability | LSP methods | Notes |
|---|---|---|
| Document sync | `textDocument/did{Open,Change,Save,Close}` | Incremental, rope-based |
| Diagnostics | `textDocument/publishDiagnostics`, `textDocument/diagnostic`, `workspace/diagnostic` | Push and pull; syntax + undefined-ref + schema + deprecations |
| Navigation | `textDocument/{definition,declaration,references,documentHighlight,documentSymbol}`, `workspace/symbol` | Cross-file |
| Hover and signatures | `textDocument/hover`, `textDocument/signatureHelp` | Version-correct function signatures, see below |
| Completion | `textDocument/completion` | Block types, schema-derived attributes, `var.*` / `local.*` / `module.*` / provider-defined functions |
| Rename | `textDocument/{prepareRename,rename}` | Cross-file, narrow identifier ranges |
| Code actions | `textDocument/codeAction`, `workspace/executeCommand` | Multi-scope quick fixes, see below |
| Formatting | `textDocument/{formatting,rangeFormatting,onTypeFormatting}` | Runtime-toggleable style, see below |
| Other navigation aids | `textDocument/{documentLink,codeLens,foldingRange,selectionRange,inlayHint}` | Registry doc links, reference counts, stale-provider hints |
| Semantic tokens | `textDocument/semanticTokens/{full,range}` | Resources, variables, references |
| Config and files | `workspace/didChangeConfiguration`, `workspace/didChangeWatchedFiles` | Live-tunable CLI timeout, debounce, format style |
| Custom | `terraform-ls/searchDocs`, `terraform-ls/getDoc`, `terraform-ls/getSnippet` | Registry doc search and retrieval, for clients that want it inline |

`workspace/executeCommand` supports three commands, each prefixed
`terraform-ls-rs.`: `initWorkspace` (runs `terraform init -backend=false`),
`fetchSchemas` (re-fetches provider schemas), and `validate` (runs
`terraform validate`).

### Diagnostics

Every rule has a stable `terraform_<name>` code you can target in the
`rules` config (see [Configuration](#configuration)). As of commit
`40cd6ff`, there are 49 rule codes, wired in
`crates/tfls-engine/src/pipeline.rs`. Five are gated behind the opt-in
`styleRules` setting (off by default): `terraform_standard_module_structure`,
`terraform_documented_variables`, `terraform_documented_outputs`,
`terraform_naming_convention`, `terraform_comment_syntax`.

Severities: `error`, `warning`, `information` (labeled `info` in config),
`hint`. Set any code to `off` to suppress it, or to another severity to
remap it.

| Code | Default severity | Checks | styleRules |
|---|---|---|---|
| `terraform_aws_renames` | warning | AWS resource type superseded by a renamed type | |
| `terraform_azurerm_blocks` | warning | Deprecated azurerm resource or block | |
| `terraform_comment_syntax` | information | Comment uses `//` instead of `#` | yes |
| `terraform_constraint` | error/warning | Version constraint malformed, or matches nothing in the registry | |
| `terraform_cyclic_locals` | error | Dependency cycle among `local` values | |
| `terraform_deprecated_index` | warning | Legacy bracket-free `list.0` index syntax | |
| `terraform_deprecated_interpolation` | warning | Unneeded `"${var.x}"` string-interpolation wrapping | |
| `terraform_deprecated_lookup` | warning | Legacy 3-argument `lookup()` call | |
| `terraform_deprecated_null_data_source` | warning | `data "null_data_source"`, part of the unmaintained `null` provider | |
| `terraform_deprecated_null_resource` | warning | `null_resource`, superseded by built-in `terraform_data` (Terraform 1.4+) | |
| `terraform_deprecated_template_dir` | warning | `data "template_dir"`, part of the unmaintained `template` provider | |
| `terraform_deprecated_template_file` | warning | `data "template_file"`, superseded by built-in `templatefile()` (Terraform 0.12+) | |
| `terraform_documented_outputs` | information | `output` block missing a `description` | yes |
| `terraform_documented_variables` | information | `variable` block missing a `description` | yes |
| `terraform_duplicate_definition` | error | Same-file duplicate block address | |
| `terraform_empty_list_equality` | warning | Comparing a list to `[]` instead of `length(...) == 0` | |
| `terraform_fmt` | information | Buffer doesn't match the active `formatStyle` | |
| `terraform_for_each_unknown_keys` | warning | `for_each` / `count` key set or `if` predicate depends on an apply-time value | |
| `terraform_google_blocks` | warning | Deprecated google resource or block | |
| `terraform_import_unknown_id` | warning | `import` block `id` / `for_each` needs a plan-known value | |
| `terraform_kubernetes_renames` | warning | `kubernetes_*` type superseded by its `_v1` equivalent | |
| `terraform_lifecycle_literal` | error | Non-literal expression in a `lifecycle` meta-argument | |
| `terraform_lock_constraint_drift` | warning | Locked provider version no longer satisfies a bumped `version` constraint | |
| `terraform_map_duplicate_keys` | error | Duplicate key in an object or map literal | |
| `terraform_meta_argument` | error/warning | `count` + `for_each` conflict, quoted `depends_on`, `for_each` over a list literal | |
| `terraform_missing_name_tag` | warning | Console-visible AWS resource with no literal `Name` tag | |
| `terraform_missing_tags` | warning | Resource whose schema has `tags`/`labels` but sets neither | |
| `terraform_module_mutable_ref` | warning | Module `source` pinned to a mutable ref instead of a tag | |
| `terraform_module_outdated` | information | Module pinned to a tag that isn't the latest known tag | |
| `terraform_module_pinned_source` | warning | Module `source` not pinned to a ref or tag | |
| `terraform_module_ref_tag_mismatch` | warning | Module's pinned tag no longer resolves to the cached commit | |
| `terraform_module_shallow_clone` | warning | Git module pinned to a ref but not using `depth=1` | |
| `terraform_module_version_presence` | warning | Registry module call missing a `version` constraint | |
| `terraform_naming_convention` | information | Block name not snake_case | yes |
| `terraform_provider_function` | error/warning | Provider-defined function call doesn't resolve or has an argument mismatch | |
| `terraform_required_providers_version` | warning | `required_providers` entry missing or misconfigured a version constraint | |
| `terraform_required_version_presence` | warning | Module missing a `terraform { required_version }` constraint | |
| `terraform_schema_validation` | error/warning | Unknown resource/data/attribute, or a deprecated one, against the provider schema | |
| `terraform_sensitive_output` | error | Sensitive value flows into an `output` not itself marked `sensitive` | |
| `terraform_standard_module_structure` | warning | Variable or output declared outside `variables.tf`/`outputs.tf` | yes |
| `terraform_syntax` | error | Parse error at its real position | |
| `terraform_tftest` | error | Structural error in a `.tftest.hcl` / `.tftest.json` file | |
| `terraform_typed_variables` | warning | `variable` block missing `type =` | |
| `terraform_undefined_reference` | warning | `var.*` / `local.*` / `module.*` reference that doesn't resolve | |
| `terraform_unused_declarations` | warning | Declared `variable` / `local` / `output` never referenced | |
| `terraform_unused_required_providers` | warning | `required_providers` entry for a provider never used in the module | |
| `terraform_variable_default_type` | error | Variable `default` shape disagrees with its declared `type` | |
| `terraform_vault_blocks` | warning | Deprecated vault resource or block | |
| `terraform_workspace_remote` | warning | `terraform.workspace` referenced while the backend is remote (HCP Terraform), where "workspace" means something else | |

### Deprecation code actions

Beyond diagnostics, several deprecations pair with a multi-scope code
action that performs the migration. The table below covers the
hand-written (tier-1) rules; every provider-flagged deprecation not in
this table still surfaces as a warning automatically (tier 2, see
below), just without an auto-fix.

| Family | Gate | Replacement | Fix |
|---|---|---|---|
| `resource "null_resource"` | Terraform >= 1.4.0 | `resource "terraform_data"` | Convert block, rewrite `null_resource.X.triggers` references workspace-wide, emit `moved {}` blocks |
| `data "template_file"` | Terraform >= 0.12.0 | `templatefile()` | Hoist to `local`, rewrite `data.template_file.X.rendered` references to `local.X`, unwrap `template = file(...)` |
| `data "template_dir"` | Terraform >= 0.12.0 | `for_each = fileset(...)` + `templatefile()` | Diagnostic only, migration is project-specific |
| `data "null_data_source"` | Terraform >= 0.10.0 | `locals {}` | Diagnostic only |
| AWS ALB family (5 types) | AWS provider >= 1.7.0 | `aws_alb*` -> `aws_lb*` | Auto-fix: rewrite labels and references, emit real `moved {}` (the types are true provider aliases, so this is always safe) |
| `aws_s3_bucket_object` (resource, data) | AWS provider >= 4.0.0 | `aws_s3_object` / `aws_s3_objects` | Auto-fix: rewrite labels and references; real `moved {}` only when `required_version` admits Terraform 1.8+, otherwise commented-out scaffolding with a header explaining why |
| `aws_kinesis_analytics_application` | AWS provider >= 4.0.0 | `aws_kinesisanalyticsv2_application` | Auto-fix, same rewrite mechanics |
| Kubernetes `_v1` family (20 types) | kubernetes provider >= 2.0.0 | append `_v1` (irregular: `kubernetes_daemonset` -> `kubernetes_daemon_set_v1`) | Auto-fix: rewrite labels and references, emit commented-out `moved {}` scaffolding with a verify-before-uncommenting header, since schemas can diverge between the unversioned and `_v1` variants |
| Azure VM split (2 types) | azurerm >= 2.40.0 | OS-specific `_linux_` / `_windows_` variants | Diagnostic only, schemas diverge |
| GCP Dataflow split | google >= 3.45.0 | `google_dataflow_flex_template_job` | Diagnostic only |
| Vault `vault_generic_secret` | vault >= 3.0.0 | `vault_kv_secret_v1` or `vault_kv_secret_v2` | Diagnostic only, target depends on the KV backend version |

Gates come in two flavours: `terraform { required_version }` for
Terraform-core deprecations, and `terraform { required_providers { <name> = ... } }`
for provider-specific ones. Both the short form (`aws = "~> 4.0"`) and
the long form (`aws = { source = "...", version = "~> 4.0" }`) are
recognised, aggregated across every file in the module.

**Tier 2, the long tail.** Beyond the table above, every resource, data
source, or attribute that a provider's own schema marks `deprecated: true`
surfaces as a warning automatically, no maintenance needed. It reads the
schema of the provider version you actually have installed, so it's
correct as that provider evolves. Suppressed for anything already
covered by the table above, so you never get warned twice for the same
thing.

Both tables are enforced by tests (`rule_table_invariants`,
`every_*_is_hardcoded_listed` in `crates/tfls-diag/src`), so a change
here can't silently drift from the code.

### Code actions across scopes

Every multi-target code action is offered at up to five scopes:

| Scope | Behaviour |
|---|---|
| Instance | Single occurrence under the cursor, or attached to a specific diagnostic |
| Selection | Every occurrence inside your visual range |
| File | Every occurrence in the active document |
| Module | Every occurrence in the active module's directory |
| Workspace | Every occurrence indexed across the workspace |

`CodeActionKind` strings are stable per action, so clients can filter via
`params.context.only`, or match by prefix to target every action this
server offers: `source.fixAll.terraform-ls-rs` alone matches all of
them; add `.<id>` for one action, `.<id>.module` or `.<id>.workspace`
for one action at one scope.

Live actions, by `<id>`: `unwrap-interpolation`, `convert-lookup-to-index`,
`set-variable-types`, `module-shallow-clone-depth`, `refine-any-types`,
`null-resource-to-terraform-data`, `template-file-to-templatefile`,
`rename-deprecated-provider-types` (drives the whole deprecation table
above), `declare-undefined-variables`, `move-outputs-to-outputs-tf`,
`move-variables-to-variables-tf`, and `format`. Module scope only
applies to the three that target a specific file (`declare-undefined-variables`,
`move-outputs-to-outputs-tf`, `move-variables-to-variables-tf`) — there's
no File/Selection variant for "move this block to another file."

A handful of git-module-ref fixes (pin a mutable ref to a SHA, fix a
stale tag comment, switch to a newer tag) attach to their diagnostic
directly as single Instance quick fixes; they don't have a scoped
variant since each fix targets a different source string.

### Signature help is version-correct

Function signatures come from `<binary> metadata functions -json`,
fetched once per session and cached on disk at
`$XDG_CACHE_HOME/terraform-ls-rs/functions/`, keyed by the binary's
canonical path and mtime. A CLI upgrade invalidates the cache
automatically. Without a CLI available, a gzipped snapshot of
OpenTofu's latest built-ins ships in the binary as a fallback.
Regenerate it with `scripts/refresh-bundled-functions.sh`.

### Formatting, two styles

The formatter wraps [`tf-format`](https://github.com/alisonjenkins/tf-format)
and exposes two runtime-toggleable styles:

- **`minimal`** (default) — `terraform fmt` / `tofu fmt` parity.
  Alignment and spacing only, source order preserved. Safe on any repo.
- **`opinionated`** — full `tf-format`: alphabetises top-level blocks,
  hoists meta-arguments, sorts attributes and object keys, expands wide
  single-line objects, adds trailing commas.

Set it in `initializationOptions.formatStyle`, live-toggle it with
`workspace/didChangeConfiguration` (`{"settings":{"terraform-ls-rs":{"formatStyle":"opinionated"}}}`),
or check in a `.tfls.json` — see [Configuration](#configuration).

## Configuration

Every key below can be set three ways, applied in this order, each
later step winning on the keys it sets:

1. A checked-in `.tfls.json` at the workspace root or any ancestor
   directory (nearest one wins). Same shape as the LSP settings object,
   with no wrapper key: `{"rules": {...}, "styleRules": true}`.
2. `initializationOptions` on the LSP `initialize` request.
3. `workspace/didChangeConfiguration`, applied live, no restart needed.

Wrap keys under `"terraform-ls-rs"` for `initializationOptions` and
`didChangeConfiguration` (`{"terraform-ls-rs": {"formatStyle": "minimal"}}`);
`.tfls.json` skips the wrapper.

| Key | Type | Default | Description |
|---|---|---|---|
| `formatStyle` | `"minimal"` \| `"opinionated"` | `"minimal"` | Active formatter style |
| `cliEnabled` | boolean | `true` | Whether the server may shell out to the Terraform/OpenTofu CLI at all |
| `cliBinary` | string | `"tofu"` | CLI binary name (on `PATH`) or path |
| `cliTimeoutSecs` | number | `60` | Timeout, in seconds, for CLI invocations |
| `watchDebounceMs` | number | `150` | Debounce for file-watch events, in milliseconds |
| `staleVersionDays` | number | `180` | Days after which a pinned provider version is flagged stale; `0` disables the check |
| `styleRules` | boolean | `false` | Enables the five style-pack rules listed above |
| `rules` | object | `{}` | Per-rule severity overrides, keyed by code: `{"terraform_naming_convention": "off"}` |
| `planKnownComputedCollections` | object | `{}` | Extends the built-in allowlist of plan-known computed collection fields, for the unknown-value diagnostics: `{"<type>.<attribute>": ["<field>", ...]}` |

`rules` and `planKnownComputedCollections` **replace the whole map** on
every update, they don't merge. A `didChangeConfiguration` payload that
sets any `rules` key discards every rule a `.tfls.json` set, not just
the overlapping ones — omit `rules` from your editor settings if you
want the project file's policy to stand untouched.

## `tfls-lint`

`tfls-lint` runs the same diagnostics engine as the editor, from the
command line, no LSP client required:

```sh
tfls-lint .
tfls-lint --format github --fail-on warning .
tfls-lint --format sarif . > tfls.sarif   # then upload with github/codeql-action/upload-sarif
```

```
tfls-lint [PATHS...]                          # default: ["."], one root per path
tfls-lint --schemas <plugins|bundled|none>    # default plugins
tfls-lint --fail-on <error|warning|info|hint|never>  # default error
tfls-lint --rule <CODE=off|hint|info|warning|error>  # repeatable
tfls-lint --style-rules                       # opt-in style pack
tfls-lint --jobs <N>                          # worker threads
tfls-lint -q / --quiet                        # summary line only
tfls-lint -v / --verbose                      # detailed logging + schema-outcome lines
tfls-lint --relative-to <DIR>                 # print paths relative to DIR
tfls-lint --format <text|json|sarif|github>   # default text
tfls-lint --config <FILE>                     # explicit .tfls.json, skips discovery
tfls-lint --no-config                         # skip .tfls.json discovery entirely
tfls-lint --offline                           # skip cache warming, see below
```

Output: one line per diagnostic to stdout, sorted by path/line/column/code:
`<relative path>:<line+1>:<col+1>: <severity> [<code>] <message>`. A
summary line goes to stderr. Exit codes: `0` clean or below
`--fail-on`, `1` findings at or above it, `2` a tool error (bad path,
load failure).

`--rule` and `--style-rules` build the same `{"rules": {...}, "styleRules": ...}`
shape `.tfls.json` and the LSP settings accept, and apply it after any
discovered `.tfls.json`, so flags win over the file.

Rules that validate against provider schemas need `terraform init` /
`tofu init` run first, or pass `--schemas bundled` to use the bundled
snapshot instead. Four rules (`terraform_constraint`,
`terraform_lock_constraint_drift`, `terraform_module_outdated`,
`terraform_module_ref_tag_mismatch`) read on-disk caches under
`$XDG_CACHE_HOME/terraform-ls-rs/` instead of fetching inline; `tfls-lint`
warms them before linting unless you pass `--offline`. Cache repeated CI
runs with:

```yaml
- uses: actions/cache@v4
  with:
    path: ~/.cache/terraform-ls-rs
    key: tfls-cache-${{ runner.os }}
```

Full internals (output-format details, cache-warming mechanics) are in
[CLAUDE.md](CLAUDE.md).

### GitHub Action

A reusable composite action (`action.yml` at the repo root) downloads
the matching `tfls-lint` release asset for the runner OS, verifies its
checksum, and runs it:

```yaml
- uses: alisonjenkins/terraform-ls-rs@v0.17.0
  with:
    fail-on: warning
```

SARIF upload for GitHub code scanning:

```yaml
- uses: alisonjenkins/terraform-ls-rs@v0.17.0
  id: tfls-lint
  with:
    fail-on: warning
    sarif-file: out/tfls.sarif
- uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: out/tfls.sarif
```

| Input | Default | Description |
|---|---|---|
| `version` | `latest` | Release tag to install (`v0.17.0`), or `latest` |
| `paths` | `.` | Whitespace-separated workspace roots |
| `format` | `github` | Output format: `text`, `json`, `sarif`, `github` |
| `fail-on` | `error` | Minimum severity that trips a non-zero exit |
| `schemas` | `plugins` | Provider schema source: `plugins`, `bundled`, `none` |
| `rules` | (empty) | Whitespace-separated `CODE=LEVEL` overrides |
| `style-rules` | `false` | Enable the opt-in style rule pack |
| `config` | (empty) | Explicit path to a `.tfls.json` |
| `no-config` | `false` | Skip `.tfls.json` discovery |
| `offline` | `false` | Skip warming the on-disk version/git-ref caches |
| `sarif-file` | (empty) | Also write a SARIF report here (always `--fail-on never`, so it never masks the main run's exit code) |
| `working-directory` | `.` | Directory to run `tfls-lint` from |
| `token` | `${{ github.token }}` | Token for the release-lookup API call only |

Outputs: `exit-code` (the main run's exit code), `sarif-file` (echoes
the input when set). No macOS runner support, no macOS build is
published.

## Performance

Seven `criterion` benchmark suites cover the hot paths: parsing and
position mapping, symbol/reference extraction, schema deserialisation,
workspace/document symbol search, signature-help context detection, and
the code-action handler's scan-and-cache pipeline. Run them yourself:

```sh
cargo bench --workspace
```

Individual suites: `crates/tfls-core/benches`, `crates/tfls-diag/benches`,
`crates/tfls-lsp/benches`, `crates/tfls-parser/benches`,
`crates/tfls-schema/benches`, `crates/tfls-state/benches`,
`crates/tfls-walker/benches`. No numbers are published here since they
depend on your hardware; commit messages on perf-focused changes cite a
before/after from the relevant suite with the commit and machine noted.

The `code_action` handler runs many independent body scans plus a full
formatter pass per request. Caching keeps that flat as workspaces grow:

- **Cross-call format cache** — each document keeps its last format
  output keyed by `(version, formatStyle)`. Repeated code-action menu
  opens on an unchanged document skip the formatter entirely.
- **Per-call scan caches** — each body-walking function caches its scan
  output across the multi-scope loop, so a fifth scope doesn't cost a
  fifth body walk.
- **Combined deprecation walker** — every deprecation reference
  rewriter shares one body iteration instead of walking once per rule.

## Development

```sh
nix develop               # fenix Rust toolchain + OpenTofu + cargo tools

cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo bench --workspace
```

Workspace `clippy` lints deny `unwrap_used`, `expect_used`, `panic`, and
`dbg_macro`. Only tests and benchmark modules `#[allow]` them.

A [prek](https://github.com/j178/prek) pre-commit hook runs `cargo fmt
--check` through the pinned Nix toolchain, so misformatted Rust can't be
committed or drift from CI. It installs automatically the first time
you enter `nix develop`.

```sh
prek run --all-files      # run the hooks over the whole tree
git commit -n              # bypass hooks for one commit, discouraged
```

Contributor and agent-facing documentation (architecture, debug
binaries, the deprecation and code-action frameworks, internal caches)
lives in [CLAUDE.md](CLAUDE.md). Investigation notes from past debugging
and design passes are indexed in [docs/README.md](docs/README.md).

## License

MPL-2.0, matching upstream `terraform-ls`.
