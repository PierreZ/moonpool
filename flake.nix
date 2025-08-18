{
  description = "Moonpool - Distributed Systems Toolbox";

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
        
        # Create rust toolchain with specified version and components
        rust-toolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
          extensions = rustComponents;
        };
        
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            # Rust toolchain from oxalica
            rust-toolchain
            
            # Development tools
            pkg-config
            openssl  
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            # macOS specific dependencies
            pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
            pkgs.darwin.apple_sdk.frameworks.CoreFoundation
          ];

          shellHook = ''
            echo "🌙 Moonpool development environment loaded"
            echo "Rust version: $(rustc --version)"
            echo "Cargo version: $(cargo --version)"
            
            # Set environment variables
            export RUST_BACKTRACE=1
            export RUST_LOG=debug
            
            # Inform about available tools
            echo "Available tools:"
            echo "  • rustc, cargo, rustfmt, clippy, rust-analyzer"
            echo "  • Use 'cargo build' to build the project"
            echo "  • Use 'cargo test' to run tests"
            echo "  • Use 'cargo fmt' to format code"
          '';

          # Environment variables
          RUST_SRC_PATH = "${rust-toolchain}/lib/rustlib/src/rust/library";
        };
      }
    );
}