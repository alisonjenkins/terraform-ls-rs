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
| Diagnostics | `textDocument/publishDiagnostics` | Syntax + undefined-ref + schema |
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
| Code actions | `textDocument/codeAction` | Quick-fix: insert missing required attrs |
| Document links | `textDocument/documentLink` | Resource/data blocks → registry docs |
| Formatting | `textDocument/formatting` | Whole document |
| Range formatting | `textDocument/rangeFormatting` | Selection only, parse-validated |
| On-type formatting | `textDocument/onTypeFormatting` | Triggered by `}` |
| Folding | `textDocument/foldingRange` | Every multi-line block |
| Selection range | `textDocument/selectionRange` | Smart expand-selection |
| Inlay hints | `textDocument/inlayHint` | Literal `default` values after `var.*` refs |
| Semantic tokens | `textDocument/semanticTokens/{full,range}` | Resources, variables, references |
| Did change configuration | `workspace/didChangeConfiguration` | Live-tunable CLI timeout, debounce, etc. |
| Did change watched files | `workspace/didChangeWatchedFiles` | Client-driven file events |
| Execute command | `workspace/executeCommand` | `initWorkspace`, `fetchSchemas`, `validate` |

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

## Performance

Real numbers (criterion on an AMD workstation, release profile):

| Benchmark | Time |
|-----------|------|
| Parse 100 resource blocks | ~417 µs |
| Extract symbols (100 blocks) | ~90 µs |
| Extract references (100 blocks) | ~76 µs |
| Deserialise 200×40 schema (sonic-rs) | ~1.6 ms |
| workspace/symbol at 10k symbols (exact) | ~206 µs |
| workspace/symbol at 10k symbols (fuzzy) | ~642 µs |
| documentSymbol at 500 symbols | ~34 µs |
| signatureHelp call-context detection (200 lines) | ~5.8 µs |

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

Recent additions:
- **Plugin protocol schema fetch** — speaks the terraform plugin gRPC
  protocol directly to provider binaries in `.terraform/providers/`,
  bypassing `tofu providers schema -json` and its credential requirements
- **Registry docs enrichment** — fills missing attribute descriptions
  (e.g. AWS SDKv2 providers) from the Terraform Registry HTTP API,
  cached to disk for subsequent runs
- **Module-aware indexing** — walks up from opened files to find the
  nearest `.terraform/providers/` directory

Not yet implemented (future work):
- Completion inside string-interpolation templates
- Provider-defined function completion (hover + signature help work,
  but no completion context for function names)

## License

MPL-2.0, matching the upstream `terraform-ls`.
