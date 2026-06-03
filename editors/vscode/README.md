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
