# terraform-ls-rs

A high-performance Rust implementation of the Terraform Language Server,
built to address the severe latency and memory issues of HashiCorp's
Go-based `terraform-ls`.

Why rewrite: `terraform-ls` regularly consumes 2–10 GB of RAM on
moderately-sized workspaces, pegs a CPU core for minutes during
indexing, and can leave stale errors on screen long after the
underlying code has been fixed. The root causes are Go's GC pressure,
synchronous `terraform` CLI calls, full re-parses on every edit, and
`go-memdb` overhead.

This project replaces those pieces with:

- **[`hcl-edit`](https://docs.rs/hcl-edit)** for HCL parsing with
  preserved position info
- **[`ropey`](https://docs.rs/ropey)** for O(log n) incremental edits
- **[`dashmap`](https://docs.rs/dashmap)** for lock-free concurrent
  state (replacing `go-memdb`)
- **[`sonic-rs`](https://docs.rs/sonic-rs)** for SIMD-accelerated JSON
  parsing of large provider schemas
- **[`tokio`](https://tokio.rs)** async everywhere — CLI schema
  fetches never block the server thread
- **[`tower-lsp`](https://docs.rs/tower-lsp)** for the LSP protocol

## Features

**31 LSP methods implemented** — covering everything `hashicorp/terraform-ls`
supports plus the features its users have been asking for (rename,
documentHighlight, foldingRange, inlayHint, on-type formatting, ...).

| Feature | Method | Notes |
|---------|--------|-------|
| Document sync | `textDocument/did{Open,Change,Save,Close}` | Incremental (rope-based) |
| Diagnostics | `textDocument/publishDiagnostics` | Syntax + undefined-ref + schema + deprecations |
| Go to definition / declaration | `textDocument/{definition,declaration}` | Cross-file |
| Find references | `textDocument/references` | Cross-file |
| Document highlight | `textDocument/documentHighlight` | Write on definition, Read on references |
| Hover | `textDocument/hover` | Kind + name |
| Completion | `textDocument/completion` | Block types, schema-derived attrs, `var.*` / `local.*` / `module.*` |
| Signature help | `textDocument/signatureHelp` | Cached version-correct function signatures |
| Rename | `textDocument/{prepareRename,rename}` | Cross-file with narrow identifier ranges |
| Document symbol | `textDocument/documentSymbol` | Outline view |
| Workspace symbol | `workspace/symbol` | Subsequence fuzzy match, ~200 µs at 10k symbols |
| Code lens | `textDocument/codeLens` | Reference counts on each definition |
| Code actions | `textDocument/codeAction` | Multi-scope quick-fixes — see below |
| Document links | `textDocument/documentLink` | Resource/data blocks → registry docs |
| Formatting | `textDocument/formatting` | Whole document, runtime-toggleable style (`minimal` / `opinionated`) |
| Range formatting | `textDocument/rangeFormatting` | Selection only, parse-validated |
| On-type formatting | `textDocument/onTypeFormatting` | Triggered by `}` |
| Folding | `textDocument/foldingRange` | Every multi-line block |
| Selection range | `textDocument/selectionRange` | Smart expand-selection |
| Inlay hints | `textDocument/inlayHint` | Literal `default` values after `var.*` refs |
| Semantic tokens | `textDocument/semanticTokens/{full,range}` | Resources, variables, references |
| Did change configuration | `workspace/didChangeConfiguration` | Live-tunable CLI timeout, debounce, format style |
| Did change watched files | `workspace/didChangeWatchedFiles` | Client-driven file events |
| Execute command | `workspace/executeCommand` | `initWorkspace`, `fetchSchemas`, `validate` |

### Deprecation diagnostics + auto-fix actions

Version-aware warnings for the major HashiCorp-provider deprecations,
each module-gated against the project's `terraform { required_version }`
constraint (a constraint in `versions.tf` correctly suppresses warnings
on its sibling files). Each deprecation pairs with a multi-scope code
action that performs the migration:

| Deprecation family | Gate | Replacement | Auto-fix |
|--------------------|------|-------------|----------|
| `resource "null_resource"` | Terraform `>= 1.4.0` | `resource "terraform_data"` | Convert block + rewrite `null_resource.X.triggers` references workspace-wide + emit `moved { }` blocks to `moved.tf` for zero-downtime state migration |
| `data "template_file"` | Terraform `>= 0.12.0` | `templatefile()` function | Hoist to `local`, rewrite `data.template_file.X.rendered` → `local.X` references, unwrap `template = file("path")` to `templatefile("path", ...)`, skip on local-name collision |
| `data "template_dir"` | Terraform `>= 0.12.0` | `for_each = fileset(...) + templatefile()` | Diagnostic only (migration project-specific) |
| `data "null_data_source"` | Terraform `>= 0.10.0` | `locals { }` block | Diagnostic only |
| AWS alb family (5 resources) | AWS provider `>= 1.7.0` | `aws_alb*` → `aws_lb*` | **Auto-fix:** rewrite labels + refs + emit `moved { }` (safe — true aliases in provider) |
| AWS `aws_s3_bucket_object` | AWS provider `>= 4.0.0` | `aws_s3_object` | **Auto-fix:** rewrite labels + refs; real `moved {}` emitted when module's `required_version` admits Terraform 1.8+ (cross-type `moved` needs CLI 1.8 + provider `MoveResourceState`). Below 1.8: commented-out scaffolding with a "REQUIRES TERRAFORM 1.8+" header pointing at either a `required_version` bump or `terraform state mv` |
| Kubernetes `_v1` rename family (20 resources) | Kubernetes provider `>= 2.0.0` | append `_v1` suffix | **Auto-fix:** rewrite labels + refs + emit COMMENTED-OUT `moved {}` scaffolding in `moved.tf` with a verify-before-uncommenting header. Schemas diverge between unversioned and `_v1` variants — user runs `terraform plan` first; if no destructive changes, uncomment the pre-written `moved` block(s); otherwise use `terraform state mv` or `terraform state rm` + `terraform import` (header explains both paths) |
| Azure VM split family (2 resources) | azurerm `>= 2.40.0` | OS-specific `_linux_` / `_windows_` variants | Diagnostic only (semantic split, schema diverges) |
| GCP Dataflow split | google `>= 3.45.0` | `google_dataflow_flex_template_job` | Diagnostic only |
| Vault `vault_generic_secret` | vault `>= 3.0.0` | `vault_kv_secret_v1` / `vault_kv_secret_v2` | Diagnostic only (target depends on KV backend version) |

**AWS rename family** (one consolidated module, one body walk per code-action call):

| From | To |
|------|----|
| `aws_alb` | `aws_lb` |
| `aws_alb_listener` | `aws_lb_listener` |
| `aws_alb_listener_rule` | `aws_lb_listener_rule` |
| `aws_alb_target_group` | `aws_lb_target_group` |
| `aws_alb_target_group_attachment` | `aws_lb_target_group_attachment` |
| `aws_s3_bucket_object` | `aws_s3_object` |

**Kubernetes `_v1` rename family** — `kubernetes_pod`, `kubernetes_deployment`, `kubernetes_service`, `kubernetes_namespace`, `kubernetes_config_map`, `kubernetes_secret`, `kubernetes_role`, `kubernetes_role_binding`, `kubernetes_cluster_role`, `kubernetes_cluster_role_binding`, `kubernetes_persistent_volume`, `kubernetes_persistent_volume_claim`, `kubernetes_service_account`, `kubernetes_stateful_set`, `kubernetes_daemonset`, `kubernetes_job`, `kubernetes_cron_job`, `kubernetes_network_policy`, `kubernetes_ingress`, `kubernetes_horizontal_pod_autoscaler` — all migrate by appending `_v1`.

**Azure split family** — `azurerm_virtual_machine` and `azurerm_virtual_machine_scale_set` each split into `_linux_` + `_windows_` variants (azurerm 2.40+).

**GCP block deprecations** — `google_dataflow_job` → `google_dataflow_flex_template_job` (google 3.45+).

Gates come in two flavours: `terraform { required_version }`
(Terraform-core deprecations) and
`terraform { required_providers { <name> = ... } }`
(provider-specific). Both forms — short `"~> 4.0"` and long
`{ source = "...", version = "~> 4.0" }` — are recognised.

**Schema-driven deprecation detection (long tail).** Beyond
the hardcoded rules above, every resource / data source / attribute
that the provider's own schema marks `deprecated: true` surfaces
as a WARNING — automatically, no maintenance. Catches the long
tail of provider deprecations (e.g. `aws_s3_bucket_object`,
`aws_alb_target_group`, `aws_db_security_group`,
`kubernetes_pod` v1, dozens of attribute renames per provider
release) without needing a hand-written rule. Suppressed on
labels covered by a hardcoded rule so users don't get
double-warned. Provider-version-correct because it reads the
*installed* provider's schema — older provider versions don't
have the deprecation flag set, newer ones do.

Multi-scope means one click can convert a single block (cursor
variant), every block in the active file, every block across the
module, or every block in the entire workspace — gated per-module so
a module pinned to an older Terraform version isn't nagged about a
feature its toolchain doesn't have.

### Code actions across scopes

Every multi-target code action is offered at five scopes:

| Scope | Behaviour |
|-------|-----------|
| **Instance** | Single occurrence under the cursor or attached to a specific diagnostic |
| **Selection** | Every occurrence inside the user's visual range |
| **File** | Every occurrence in the active document |
| **Module** | Every occurrence in the active module's directory |
| **Workspace** | Every occurrence indexed across the workspace |

LSP `CodeActionKind` strings are stable per action — clients can filter
via `params.context.only`. Examples: `source.fixAll.terraform-ls-rs.unwrap-interpolation`,
`source.fixAll.terraform-ls-rs.null-resource-to-terraform-data.workspace`.

Live actions: unwrap deprecated interpolation, convert deprecated
`lookup()` to index notation, set inferred variable types, refine
`type = any`, declare undefined variables, move outputs to `outputs.tf`,
move variables to `variables.tf`, plus the four deprecation migrations
above.

### Signature help is version-correct

The function signatures shown in signature help come from
`<binary> metadata functions -json`, fetched once per session and
cached on disk at `$XDG_CACHE_HOME/terraform-ls-rs/functions/`, keyed
by the binary's canonical path + mtime. A CLI upgrade invalidates the
cache automatically. If no CLI is available, a gzipped snapshot of
OpenTofu's latest built-ins is compiled into the binary as a fallback.

Regenerate the bundled snapshot with:

```sh
scripts/refresh-bundled-functions.sh
```

### Formatting, two styles

The formatter wraps [`tf-format`](https://github.com/alisonjenkins/tf-format)
and exposes two runtime-toggleable styles:

- **`minimal`** (default) — `terraform fmt` / `tofu fmt` parity.
  Alignment + spacing only; source order preserved. Safe to apply
  to any repo.
- **`opinionated`** — full `tf-format`: alphabetises top-level
  blocks, hoists meta-arguments, sorts attributes/object keys,
  expands wide single-line objects, adds trailing commas.

Toggle live via `workspace/didChangeConfiguration` with
`{"settings":{"terraform-ls-rs":{"formatStyle":"opinionated"}}}`
or set initially via `initializationOptions.formatStyle`.

## Performance

Real numbers (criterion on an AMD workstation, release profile):

| Benchmark | Time |
|-----------|------|
| Parse 100 resource blocks | ~417 µs |
| Extract symbols (100 blocks) | ~90 µs |
| Extract references (100 blocks) | ~76 µs |
| Deserialise 200×40 schema (sonic-rs) | ~1.6 ms |
| `workspace/symbol` at 10k symbols (exact) | ~206 µs |
| `workspace/symbol` at 10k symbols (fuzzy) | ~642 µs |
| `documentSymbol` at 500 symbols | ~34 µs |
| `signatureHelp` call-context detection (200 lines) | ~5.8 µs |
| `code_action` against 500-block deprecation fixture | ~11 ms (was 70 ms before caching pass) |
| Deprecation diagnostic walk (1000 blocks) | ~350 µs |

The `code_action` handler runs many independent body scans + a full
formatter pass per request. Several layers of caching keep latency
flat as workspaces grow:

- **Cross-call format cache** — `DocumentState` carries the last
  format-output keyed by `(version, FormatStyle)`. Repeated
  code-action menu opens on an unchanged doc skip the formatter
  entirely; invalidated automatically by `apply_change`.
- **Per-call scan caches** — every emit fn that walks the body
  caches its scan output across the multi-scope loop, so adding a
  fifth scope iteration doesn't cost a fifth body walk.
- **Combined deprecation walker** — every deprecation
  reference rewriter shares one body iteration via a
  `HashMap<Url, CombinedDeprecationRefs>` populated lazily by the
  first emit fn that needs it.

Net effect on the synthetic 500-block worst-case fixture: **70 ms →
~11 ms (-84%)** since the first commit on this branch.

## Install

### Using Nix (recommended)

The flake provides a package, a dev-shell with all build and test
dependencies, and pre-commit-style checks.

```sh
# Run it once without installing
nix run github:your-org/terraform-ls-rs

# Install into your profile
nix profile install github:your-org/terraform-ls-rs

# Drop into a dev shell with fenix-managed Rust + opentofu + rust-analyzer
nix develop
```

### Using Cargo

```sh
cargo install --path crates/tfls-cli
```

The binary is called `tfls`.

## Editor setup

### Neovim (with `nvim-lspconfig`)

```lua
local configs = require('lspconfig.configs')
if not configs.tfls then
  configs.tfls = {
    default_config = {
      cmd = { 'tfls' },
      filetypes = { 'terraform', 'terraform-vars' },
      root_dir = require('lspconfig.util').root_pattern('*.tf', '.git'),
    },
  }
end
require('lspconfig').tfls.setup {}
```

### VS Code

Install via a generic "LSP client" extension (e.g. `ms-vscode.vscode-lsp`)
pointed at the `tfls` binary, or build a small wrapper extension that
spawns `tfls` on stdio. A dedicated extension is not yet published.

## Development

```sh
nix develop               # fenix Rust toolchain + opentofu + cargo tools

cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo bench               # criterion benchmarks
```

Build-level guarantees enforced by the workspace `clippy` lints:

- `unwrap_used = "deny"`
- `expect_used = "deny"`
- `panic = "deny"`
- `dbg_macro = "deny"`

The only exceptions are tests and benchmark modules, which explicitly
`#[allow]` those lints.

## Architecture

Nine-crate Cargo workspace, each with its own `thiserror` error enum
and `#[source]` chain preservation:

```
crates/
  tfls-core/               domain types (Symbol, ProviderAddress, ...)
  tfls-parser/             hcl-edit wrapper, position mapping, symbol + ref extraction
  tfls-schema/             provider schema types + async CLI fetcher
  tfls-state/              StateStore (DashMap), DocumentState (rope + AST), JobQueue
  tfls-diag/               syntax, undefined-ref, schema-validation diagnostics
  tfls-format/             formatter
  tfls-walker/             fs discovery + notify-debouncer-full file watcher
  tfls-provider-protocol/  terraform plugin gRPC protocol (v5+v6), mTLS, registry docs
  tfls-lsp/                Backend (tower-lsp) + handlers + background indexer
  tfls-cli/                main: tokio, clap, stdio transport
```

On `initialize`, the Backend spawns:

1. a **worker task** draining the priority job queue,
2. a **file watcher** per workspace folder forwarding FS events as
   Normal-priority jobs, and
3. a one-shot **schema fetch** — prefers the plugin gRPC protocol
   (speaking directly to provider binaries in `.terraform/providers/`,
   no credentials required), falling back to `tofu providers schema -json`
   if no cached providers exist.

The queue deduplicates identical jobs and delivers by priority
(`Immediate > High > Normal > Low`), so a flood of save events for the
same file collapses into a single re-parse.

When a file parse fails mid-keystroke, the document's last-good
symbol table is retained so completion and navigation keep working.

## Status

Every documented feature has integration tests. The binary runs, the
Nix flake builds, and the server talks real JSON-RPC LSP to test
clients.

Highlights:

- **34 deprecation diagnostics live** across 4 Terraform-core
  rules + AWS / Kubernetes / Azure / GCP / Vault provider
  families. Each is module-aware (sibling `versions.tf` /
  `required_providers` constraints suppress correctly) and
  scales atop a generic `DeprecationRule` framework. Adding
  another rename to a provider family is one table entry;
  adding a different-shape deprecation is ~25 lines of new
  module. Both Terraform-core and provider-version gates are
  supported.
- **Auto-fix for 30+ deprecation rules** — multi-scope
  (Selection / File / Module / Workspace), cursor-driven
  Instance variant, and diagnostic-attached lightbulb
  quickfix all surfaced through the same `BlockRenameSpec`
  framework. Per-spec migration safety classification
  (`Aliased` / `RequiresTerraform18` / `Manual`) governs
  whether `moved {}` blocks emit as real Terraform syntax
  or as commented-out scaffolding with verify-before-uncommenting
  headers — no resource rename ever ships a silently-dangerous
  state-migration emit.
- **Tier-2 schema-driven catch-all** — every resource / data
  source / attribute the provider's schema marks
  `deprecated: true` surfaces as a WARNING automatically.
  Suppressed on labels covered by tier-1 (richer message +
  auto-fix). Provider-version-correct by construction (reads
  the *installed* provider's schema).
- **Curation tool** — `tfls-deprecation-scrape` walks an
  initialised workspace's `.terraform/providers/` and surfaces
  uncovered deprecation candidates worth promoting to tier-1.
  Rust scaffolding output mode emits a draft `DeprecationRule`
  module for any chosen type.
- **Multi-scope code actions** — Instance / Selection / File /
  Module / Workspace, with stable `CodeActionKind` strings clients
  can filter on.
- **Plugin protocol schema fetch** — speaks the Terraform plugin gRPC
  protocol directly to provider binaries in `.terraform/providers/`,
  bypassing `tofu providers schema -json` and its credential
  requirements.
- **Registry docs enrichment** — fills missing attribute descriptions
  (e.g. AWS SDKv2 providers) from the Terraform Registry HTTP API,
  cached to disk for subsequent runs.
- **Module-aware indexing** — walks up from opened files to find the
  nearest `.terraform/providers/` directory.

Not yet implemented (future work):

- Completion inside string-interpolation templates
- Provider-defined function completion (hover + signature help work,
  but no completion context for function names)
- More provider-version-gated deprecations (`aws_s3_bucket_object` →
  `aws_s3_object`, `aws_alb_target_group` → `aws_lb_target_group`,
  …). Framework now supports the gate kind; pull requests welcome.

## License

MPL-2.0, matching the upstream `terraform-ls`.
