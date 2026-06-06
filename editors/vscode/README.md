# Terraform / OpenTofu for VS Code (terraform-ls-rs)

Terraform & OpenTofu language support powered by [`tfls`](https://github.com/alisonjenkins/terraform-ls-rs),
a fast Rust language server.

## Features

Completion, hover, go-to-definition / references, document & workspace symbols,
diagnostics, code actions (quick fixes), semantic highlighting, folding, rename,
signature help, inlay hints, code lens, and formatting — `minimal` (`terraform fmt`
parity) or `opinionated`.

## Server binary

On first activation the extension downloads the matching `tfls` release for your
platform (Linux x64, macOS arm64, Windows x64) from
[GitHub releases](https://github.com/alisonjenkins/terraform-ls-rs/releases),
verifies its checksum, and caches it.

To use your own build instead, set:

```jsonc
"terraform-ls-rs.serverPath": "/path/to/tfls"
```

## Code actions

Two ways to invoke them (VS Code splits them by kind):

- **Quick fixes** (convert `null_resource` → `terraform_data`, set a variable
  type, add `depth=1`, unwrap interpolation, …) — put the cursor on the line and
  press **`Ctrl+.`** (macOS **`Cmd+.`**), or click the 💡 lightbulb.
- **Scoped "source" actions** — the same fixes applied across a wider scope
  (**File / Module / Workspace**) are `source.*` actions, which `Ctrl+.` hides.
  Run them via **Command Palette → "Source Action…"**.

Each scoped action has a stable kind: `source.fixAll.terraform-ls-rs.<id>` plus
`.module` / `.workspace` variants (`<id>` e.g. `set-variable-types`,
`convert-lookup-to-index`, `module-shallow-clone-depth`, `unwrap-interpolation`).

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

All under the `terraform-ls-rs.*` namespace — see the Settings UI. Highlights:
`serverPath`, `formatStyle`, `cliBinary`, `styleRules`, and `rules` (per-rule
severity overrides).

## Coexistence

If you also have the official HashiCorp Terraform extension installed, disable one
of them per-workspace so a single server owns formatting and diagnostics.
