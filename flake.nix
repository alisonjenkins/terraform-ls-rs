{
  description = "terraform-ls-rs — a high-performance Rust replacement for terraform-ls";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane = {
      url = "github:ipetkov/crane";
    };
  };

  outputs =
    { nixpkgs
    , flake-utils
    , fenix
    , crane
    , ...
    }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ fenix.overlays.default ];
        };

        rustToolchain = pkgs.fenix.stable.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
        ];

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Include Cargo sources plus runtime assets compiled into the
        # binary via include_bytes! (the bundled function signatures).
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            (craneLib.fileset.commonCargoSources ./.)
            ./schemas
            # Vendored tfplugin protobuf definitions consumed by
            # tfls-provider-protocol/build.rs via tonic-build.
            ./crates/tfls-provider-protocol/proto
            # Frozen registry-doc markdown loaded by integration tests via
            # include_str!; without these the sandboxed test/clippy builds
            # (cargoTest / cargoClippy --all-targets) fail to compile.
            ./crates/tfls-provider-protocol/tests/fixtures
          ];
        };

        commonArgs = {
          inherit src;
          strictDeps = true;

          nativeBuildInputs = with pkgs; [
            pkg-config
            protobuf # tonic-build needs protoc for the tfplugin6 protos
          ];

          buildInputs = with pkgs; [
            openssl
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ];

          # tonic-build reads PROTOC at build time; make it explicit so the
          # sandboxed nix build uses the pinned protobuf package rather than
          # whatever (if anything) is on $PATH.
          PROTOC = "${pkgs.protobuf}/bin/protoc";

          # Several unit tests construct a reqwest::Client eagerly (before any
          # offline doc lookup). reqwest's TLS backend aborts at construction if
          # no system CA bundle is present, which the build sandbox lacks — point
          # it at the cacert bundle so `cargoTest` runs offline.
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        tfls = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "terraform-ls-rs";
          cargoExtraArgs = "--package tfls-cli";
          meta = with pkgs.lib; {
            description = "High-performance Rust Terraform language server";
            license = licenses.mpl20;
            mainProgram = "tfls";
          };
        });
      in
      {
        checks = {
          inherit tfls;

          tfls-clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
          });

          tfls-fmt = craneLib.cargoFmt {
            src = commonArgs.src;
          };

          tfls-tests = craneLib.cargoTest (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        packages = {
          default = tfls;
          tfls = tfls;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = tfls;
          name = "tfls";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ tfls ];

          packages = with pkgs; [
            rustToolchain
            rust-analyzer
            opentofu
            cargo-watch
            cargo-nextest
            cargo-edit
            cargo-audit
            cargo-deny
            # sccache: cache rustc invocations across `cargo build`
            # / `cargo test` runs in this shell. Configured via
            # the `env` block below to point at
            # `~/.cache/sccache`. First entry into the shell on a
            # cold cache compiles normally; subsequent rebuilds
            # of unchanged crates short-circuit through the cache.
            #
            # Stats: `sccache --show-stats`. Wipe: `rm -rf
            # ~/.cache/sccache`.
            sccache
          ];

          env = {
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            RUST_BACKTRACE = "1";
            # Wire sccache as the rustc front-end. `cargo` honours
            # `RUSTC_WRAPPER` for every rustc invocation it would
            # otherwise spawn directly.
            RUSTC_WRAPPER = "${pkgs.sccache}/bin/sccache";
            # cargo's own incremental compilation fights with
            # sccache's cache (sccache caches per-translation-unit
            # outputs that cargo would otherwise overwrite via
            # incremental stamps); disabling incremental lets
            # sccache do its job. sccache docs flag this as the
            # required combo.
            CARGO_INCREMENTAL = "0";
          };
        };
      });
}
