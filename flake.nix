{
  description = "Moonpool - deterministic simulation testing for distributed systems";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        
        # Read rust toolchain version from rust-toolchain.toml
        toolchainFile = builtins.fromTOML (builtins.readFile ./rust-toolchain.toml);
        rustVersion = toolchainFile.toolchain.channel;
        rustComponents = toolchainFile.toolchain.components or [];
        rustTargets = toolchainFile.toolchain.targets or [];

        # Create rust toolchain with specified version, components, and targets
        rust-toolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
          extensions = rustComponents;
          targets = rustTargets;
        };
        
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            # Rust toolchain from oxalica
            rust-toolchain

            # gRPC example: tonic-prost-build shells out to protoc
            protobuf

            # Development tools
            pkg-config
            openssl
            cargo-nextest
            cargo-edit

            # wasm demo: generate JS/TS bindings for the cdylib. Its version (from
            # nixpkgs) MUST match the `wasm-bindgen` crate pin in
            # crates/moonpool-wasm-demo/Cargo.toml exactly — a mismatch breaks the bindgen
            # step with an opaque "schema version" error. When `nix flake update`
            # moves this, re-pin the crate to `wasm-bindgen --version`.
            wasm-bindgen-cli

            # mdbook
            mdbook
            mdbook-toc
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
            # C toolchain for linking. Linux-only: darwin's stdenv already
            # provides clang, and GNU gcc is a heavy, fragile build there.
            gcc
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            # Crate build scripts link against iconv on darwin.
            libiconv
          ];

          shellHook = ''
            echo "🌙 Moonpool development environment loaded"
            echo "Rust version: $(rustc --version)"
            echo "Cargo version: $(cargo --version)"

            # Set environment variables
            export RUST_BACKTRACE=1
            export RUST_LOG=debug
            # Use the flake source path, not $PWD: `nix develop` is valid from
            # any workspace subdirectory.
            export RUSTC_WRAPPER="${self}/scripts/sancov-rustc.sh"
            
            # Inform about available tools
            echo "Available tools:"
            echo "  • rustc, cargo, rustfmt, clippy, rust-analyzer"
            echo "  • cargo-nextest for better test management"
            echo "  • Use 'cargo build' to build the project"
            echo "  • Use 'cargo test' to run tests"
            echo "  • Use 'cargo nextest run' for better test output with timeouts"
            echo "  • Use 'cargo fmt' to format code"
          '';

          # Environment variables
          RUST_SRC_PATH = "${rust-toolchain}/lib/rustlib/src/rust/library";
        };
      }
    );
}
