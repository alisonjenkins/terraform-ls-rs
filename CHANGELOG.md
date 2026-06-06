# Changelog

## [0.6.0](https://github.com/alisonjenkins/terraform-ls-rs/compare/v0.5.2...v0.6.0) (2026-06-06)


### Features

* **code-action:** add depth=1 to pinned git module sources ([5c55ee7](https://github.com/alisonjenkins/terraform-ls-rs/commit/5c55ee72ab635ff5c0f30591418e443dc0dfb1ca))


### Bug Fixes

* **diag:** stop false-positive enum warning from quoted allowed values ([1adc586](https://github.com/alisonjenkins/terraform-ls-rs/commit/1adc586236f1b4170eb932bbda6793d2bc60f960))
* **lsp:** on-type/range formatting no longer inserts a blank line ([1f9773e](https://github.com/alisonjenkins/terraform-ls-rs/commit/1f9773e4bd20c8ecbadf4090e6dc08279e300911))
* **lsp:** strip provider prefix from registry doc-link URLs ([e72b567](https://github.com/alisonjenkins/terraform-ls-rs/commit/e72b5679fc9fce61a2ed7fdc8bebacedb48fdf6a))

## [0.5.2](https://github.com/alisonjenkins/terraform-ls-rs/compare/v0.5.1...v0.5.2) (2026-06-05)


### Bug Fixes

* **lsp:** scope resource/data document links to the type label ([9b91a4a](https://github.com/alisonjenkins/terraform-ls-rs/commit/9b91a4a8d67e7f3e8b1f0591a1a62661d0320458))

## [0.5.1](https://github.com/alisonjenkins/terraform-ls-rs/compare/v0.5.0...v0.5.1) (2026-06-05)


### Bug Fixes

* **ci:** build linux release as static musl binary ([8eaee7f](https://github.com/alisonjenkins/terraform-ls-rs/commit/8eaee7fb101f241a3db405cee6c384bd4c10f53f))

## [0.5.0](https://github.com/alisonjenkins/terraform-ls-rs/compare/v0.4.2...v0.5.0) (2026-06-05)


### Features

* **vscode:** add Terraform/HCL syntax highlighting grammar ([bdab242](https://github.com/alisonjenkins/terraform-ls-rs/commit/bdab2427c5eebb0d7eac106c1cbffe7fb613c893))

## [0.4.2](https://github.com/alisonjenkins/terraform-ls-rs/compare/v0.4.1...v0.4.2) (2026-06-05)


### Bug Fixes

* **vscode:** bump extension version with each release ([ad05bfa](https://github.com/alisonjenkins/terraform-ls-rs/commit/ad05bfad5b3b36bfc9df8e6ed3f7c0834dceecfb))

## [0.4.1](https://github.com/alisonjenkins/terraform-ls-rs/compare/v0.4.0...v0.4.1) (2026-06-05)


### Bug Fixes

* **ci:** publish releases via draft flow for immutable releases ([b480905](https://github.com/alisonjenkins/terraform-ls-rs/commit/b4809056a3617d178f353668729dba27b1f49f69))

## [0.4.0](https://github.com/alisonjenkins/terraform-ls-rs/compare/v0.3.0...v0.4.0) (2026-06-05)


### Features

* migrate off unmaintained tower-lsp to tower-lsp-server ([c814749](https://github.com/alisonjenkins/terraform-ls-rs/commit/c814749c34d0027882cc06b545c3e74988327eca))


### Bug Fixes

* **vscode:** repair + modernise the extension toolchain ([99f8448](https://github.com/alisonjenkins/terraform-ls-rs/commit/99f84485366c825af666fcea55dd0702e3a75af8))

## [0.3.0](https://github.com/alisonjenkins/terraform-ls-rs/compare/v0.2.2...v0.3.0) (2026-06-05)


### Features

* **schema:** bundle built-in terraform provider for completion + hover ([b500e19](https://github.com/alisonjenkins/terraform-ls-rs/commit/b500e1942815135ecb61a36f25f4bef600d2bb58))
* **vscode:** add the terraform-ls-rs VS Code extension ([178226e](https://github.com/alisonjenkins/terraform-ls-rs/commit/178226e07ea97ba472e33483eec75d3424843adc))


### Bug Fixes

* **ci:** drop --locked from the release build ([2a09347](https://github.com/alisonjenkins/terraform-ls-rs/commit/2a09347700773f22674b1f9994e8f536cda8bffe))
* **cli:** default log path to the platform temp dir ([b1106dd](https://github.com/alisonjenkins/terraform-ls-rs/commit/b1106dd7a69200125a266189765bea04e7051720))
* **provider-protocol:** compile on Windows by gating the unix transport ([4590278](https://github.com/alisonjenkins/terraform-ls-rs/commit/4590278c7beb5efc857fb6d24a6976f4f9b22aa7))
