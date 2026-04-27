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

Use this when investigating LSP message-routing bugs that span multiple client connections (lspmux dedupe, fanout, late-attach republish). Daemon stderr is captured to `<tmp>/lspmux.stderr.log` for post-mortem.

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

## Code-action scopes

Every multi-target code action (unwrap interpolation, convert lookup, set variable types, refine `type = any`, declare undefined variables, move outputs to `outputs.tf`, move variables to `variables.tf`) is offered at multiple scopes via `crates/tfls-lsp/src/handlers/code_action_scope.rs`:

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
