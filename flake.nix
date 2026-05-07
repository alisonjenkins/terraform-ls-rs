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
    { self
    , nixpkgs
    , flake-utils
    , fenix
    , crane
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

        # sccache-wrapped build for fast iteration. Most useful when
        # paired with `nix build .#tfls-sccache --impure` plus a
        # SCCACHE_* environment passed in (SCCACHE_DIR, SCCACHE_BUCKET,
        # SCCACHE_REDIS_ENDPOINT, …). With a populated SCCACHE_DIR
        # warm-cache rebuilds skip individual rustc compilations.
        #
        # Caveats: nix sandboxing means SCCACHE_DIR ends up isolated
        # per-build unless you explicitly bind-mount or run with
        # `--impure` so the build inherits your user environment.
        # Crane's `cargoArtifacts` already caches dep builds, so the
        # marginal benefit of sccache in a clean nix build is small —
        # the bigger win comes from sccache in the devShell, where
        # `cargo build` runs outside the sandbox.
        commonArgsSccache = commonArgs // {
          # `cargo` invocations during the build will spawn `rustc`
          # via this wrapper. The sccache binary must be on PATH;
          # add to nativeBuildInputs alongside the other tools.
          RUSTC_WRAPPER = "${pkgs.sccache}/bin/sccache";
          # sccache + cargo's own incremental cache fight over
          # the same artefacts and the result is slower than either
          # alone. Disable cargo incremental — sccache is the cache.
          CARGO_INCREMENTAL = "0";
          nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ [
            pkgs.sccache
          ];
        };

        cargoArtifactsSccache = craneLib.buildDepsOnly commonArgsSccache;

        tfls-sccache = craneLib.buildPackage (commonArgsSccache // {
          cargoArtifacts = cargoArtifactsSccache;
          pname = "terraform-ls-rs-sccache";
          cargoExtraArgs = "--package tfls-cli";
          meta = with pkgs.lib; {
            description = "terraform-ls-rs built with sccache as RUSTC_WRAPPER";
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
          # `nix build .#tfls-sccache` — same binary as `tfls` but
          # built with sccache wrapping rustc. Use with `--impure`
          # plus exported `SCCACHE_*` env vars when iterating
          # locally so the warm cache survives across nix builds:
          #
          #   export SCCACHE_DIR=~/.cache/sccache
          #   nix build .#tfls-sccache --impure
          tfls-sccache = tfls-sccache;
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
