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

## TODO

### Bugs / Stale Code

- [x] **Fix examples crypto provider** — `single_fetch.rs:15` and `fetch_local.rs:23` use `rustls::crypto::ring::default_provider()` but Cargo.toml only has `aws_lc_rs`. These crash at runtime. `probe.rs` is correct — copy its pattern.

- [x] **Fix stale build.rs comment** — `crates/tfls-provider-protocol/build.rs:3-4` says "Only tfplugin6 is compiled for now" but both v5 and v6 are fully compiled and supported.

- [x] **Update README.md "Status" section** — Listed done features as "future work". Also updated architecture section to mention plugin protocol and the tfls-provider-protocol crate.

- [x] **Remove dead code markers in client.rs** — `_kind_marker` and `_proto_marker` placeholders with `#[allow(dead_code)]` plus the unused `StringKind` and `proto` imports. Were just dead-code suppressors, not unfinished features.

### Unfinished Features

- [x] **`exactly_one_of` / `at_least_one_of` diagnostics** — Added checks in `schema_validation.rs` with 4 tests.

- [x] **Wire provider-defined functions into indexer** — `fetch_provider_functions()` now called after plugin protocol schema fetch; `merge_functions()` added to StateStore to avoid clearing built-ins.

- [x] **Function name completion** — Added `FunctionCall` context in `completion.rs` triggered by expression-starting tokens (`=`, `(`, `,`, `${`, operators); `function_name_items()` returns all known functions with signatures and docs.

### Test Coverage Gaps

- [x] **`client.rs` tests** — 3 tests for connect_channel error paths (missing cert, invalid base64, unreachable socket).

- [x] **`tls.rs` tests** — 11 tests covering cert generation (PEM format, DER non-empty, uniqueness), all 4 base64 decoding variants, invalid base64 rejection, pinned verifier accept/reject, and ALPN h2 config.

- [x] **`translate_v5.rs` tests** — 11 tests covering schema translation, attribute flags, empty constraints, nesting mode variants, string kind mapping, and function signature parsing.
