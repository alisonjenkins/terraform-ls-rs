# Terraform / OpenTofu for VS Code (terraform-ls-rs)

Terraform and OpenTofu language support powered by
[`tfls`](https://github.com/alisonjenkins/terraform-ls-rs), a fast Rust
language server.

## Features

Completion, hover, go-to-definition / references, document and workspace
symbols, diagnostics (both push and pull — `textDocument/diagnostic` and
`workspace/diagnostic`), code actions (quick fixes), semantic highlighting,
folding, rename, signature help, inlay hints, code lens, and formatting —
`minimal` (`terraform fmt` parity) or `opinionated`.

## Server binary

On first activation the extension downloads the matching `tfls` release for
your platform from
[GitHub releases](https://github.com/alisonjenkins/terraform-ls-rs/releases),
verifies its checksum, and caches it. Published platforms today:
**Linux x64** and **Windows x64**. There is no macOS build. On macOS, build
`tfls` yourself (see the root
[README's install section](https://github.com/alisonjenkins/terraform-ls-rs#install) —
Nix or `cargo install`) and point the extension at it:

```jsonc
"terraform-ls-rs.serverPath": "/path/to/tfls"
```

## Project config (`.tfls.json`)

A repo can check in one `.tfls.json` at its root (or any ancestor directory)
to set `rules`, `styleRules`, and `formatStyle` for everyone who opens it,
editor and CI alike, instead of each person configuring their own editor:

```json
{ "rules": { "terraform_naming_convention": "off" }, "styleRules": true }
```

Precedence: `.tfls.json` applies first, then your VS Code settings
(`initializationOptions`), then anything you change live through
`workspace/didChangeConfiguration`. Each later step wins on the keys it
sets. `rules` replaces the whole map at each step, so a VS Code setting
that sets any `rules` key discards every rule `.tfls.json` set, not just
the overlapping ones — leave `terraform-ls-rs.rules` empty in your user
settings if you want the project file's policy to stand.

## Code actions

Two ways to invoke them (VS Code splits them by kind):

- **Quick fixes** (convert `null_resource` → `terraform_data`, set a
  variable type, add `depth=1`, unwrap interpolation, …) — put the cursor
  on the line and press **`Ctrl+.`** (macOS **`Cmd+.`**), or click the
  lightbulb.
- **Scoped "source" actions** — the same fixes applied across a wider
  scope (**File / Module / Workspace**) are `source.*` actions, which
  `Ctrl+.` hides. Run them via **Command Palette → "Source Action…"**.

Every scoped action's `CodeActionKind` is `source.fixAll.terraform-ls-rs.<id>`,
with `.module` / `.workspace` suffixes for those scopes (no suffix means
File scope). VS Code and most clients match action kinds **by prefix**, so
`"source.fixAll.terraform-ls-rs"` alone (no `<id>`) matches every action
this server offers — useful for a blanket "fix everything on save," or add
the full `.terraform-ls-rs.<id>` path to target one action. `<id>` examples:
`set-variable-types`, `convert-lookup-to-index`, `module-shallow-clone-depth`,
`unwrap-interpolation`, `rename-deprecated-provider-types`.

Bind a key to a scope, or run on save:

```jsonc
// keybindings.json — apply a workspace-wide fix on demand
{
  "key": "ctrl+alt+w",
  "command": "editor.action.codeAction",
  "args": { "kind": "source.fixAll.terraform-ls-rs", "apply": "first" }
}
```
```jsonc
// settings.json — fix on save (kind is a prefix; narrow as needed)
"editor.codeActionsOnSave": {
  "source.fixAll.terraform-ls-rs.module-shallow-clone-depth": "explicit"
}
```

## Commands

- **Terraform: Toggle Format Style** — flip `minimal` ↔ `opinionated` live.
- **Terraform: Restart Language Server**
- **Terraform: Show Language Server Output**

## Settings

All under the `terraform-ls-rs.*` namespace.

| Setting | Type | Default | Description |
|---|---|---|---|
| `serverPath` | string | `""` | Absolute path to a `tfls` binary. Empty downloads the matching release automatically. A leading `~` expands to your home directory. |
| `formatStyle` | `"minimal"` \| `"opinionated"` | `"minimal"` | Formatting style applied by the language server. |
| `cliEnabled` | boolean | `true` | Allow the server to shell out to the Terraform/OpenTofu CLI for version-aware features. |
| `cliBinary` | string | `"tofu"` | Name (on `PATH`) or path of the CLI binary the server invokes. |
| `cliTimeoutSecs` | number | `60` | Timeout, in seconds, for CLI invocations. |
| `watchDebounceMs` | number | `150` | Debounce, in milliseconds, applied to file-watch events. |
| `staleVersionDays` | number | `180` | Age, in days, after which an installed provider version is flagged as stale. |
| `styleRules` | boolean | `false` | Enable the opt-in style rule pack (documented-variables, documented-outputs, naming-convention, comment-syntax) alongside correctness diagnostics. |
| `rules` | object | `{}` | Per-rule severity overrides, keyed by rule code. Example: `{ "terraform_naming_convention": "off" }`. Replaces the whole map on change; see the `.tfls.json` note above. |
| `trace.server` | `"off"` \| `"messages"` \| `"verbose"` | `"off"` | Trace the JSON-RPC traffic between VS Code and the language server. |

See the Settings UI for the same list with live validation.

## Linting outside the editor

The same diagnostics engine is available as a standalone binary,
`tfls-lint`, for CI or a pre-commit hook — no editor required. See the
root README's [`tfls-lint`](https://github.com/alisonjenkins/terraform-ls-rs#tfls-lint) section.

## Coexistence

If you also have the official HashiCorp Terraform extension installed,
disable one of them per-workspace so a single server owns formatting and
diagnostics.
