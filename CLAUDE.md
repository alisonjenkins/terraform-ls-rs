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
  tfls-format/             Formatter (parse-validated, idempotent)
  tfls-walker/             FS discovery + notify-debouncer-full file watcher
  tfls-provider-protocol/  Terraform plugin gRPC protocol (v5+v6), mTLS, registry docs
  tfls-lsp/                Backend (tower-lsp) + handlers + background indexer
  tfls-cli/                main: tokio, clap, stdio transport
```

Schema fetch has two paths:
1. **Plugin protocol** (primary) — speaks gRPC to provider binaries in `.terraform/providers/`, no credentials needed
2. **CLI fallback** — `tofu providers schema -json` when no `.terraform/providers/` exists

Registry docs enrichment fills missing attribute descriptions (e.g. AWS SDKv2 providers) from the Terraform Registry HTTP API, cached to `$XDG_CACHE_HOME/terraform-ls-rs/provider-docs/`.
