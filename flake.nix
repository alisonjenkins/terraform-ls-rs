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
          ];

          env = {
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            RUST_BACKTRACE = "1";
          };
        };
      });
}
