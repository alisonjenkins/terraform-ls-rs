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

- [ ] **Remove dead code markers in client.rs** (lines 265-275) — `_kind_marker` and `_proto_marker` placeholders with `#[allow(dead_code)]` plus the unused `StringKind` import. Either wire them up or delete them.

### Unfinished Features

- [ ] **`exactly_one_of` / `at_least_one_of` diagnostics** — Fields exist on `AttributeSchema` and are surfaced in hover (`hover_attribute.rs:265-266`), but `schema_validation.rs` doesn't generate diagnostics for them. Only `conflicts_with` and `required_with` have checks.

- [ ] **Wire provider-defined functions into indexer** — `client::fetch_provider_functions()` is implemented but never called from `indexer.rs`. Functions currently only come from the CLI `metadata functions -json` path. Provider-defined functions (e.g. `provider::aws::arn_parse`) won't appear until this is connected.

- [ ] **Function name completion** — `hover_function.rs` and `signature_help.rs` work for functions, but `completion.rs` has no `FunctionName` context. Users can't get completion suggestions when typing function names.

### Test Coverage Gaps

- [ ] **`client.rs` tests** — Zero unit tests for the core RPC logic (v5/v6 branching, function extraction, 256MB decode cap).

- [ ] **`tls.rs` tests** — Zero unit tests for certificate generation, base64 decoding variants, or pinned verifier logic.

- [ ] **`translate_v5.rs` tests** — Zero unit tests (mirrors `translate.rs` which has 2 tests).

### Polish

- [ ] **README architecture section** (lines 179-186) — Still describes schema fetch as "via `tofu providers schema -json`". Should mention the plugin protocol as the primary mechanism.
