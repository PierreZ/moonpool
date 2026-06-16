# moonpool-wasm-demo

Browser/wasm demo embedded in the book (`book/src/wasm-demo/`). Built by
`book/build-wasm-demo.sh`: `cargo build --target wasm32-unknown-unknown` then
`wasm-bindgen` to generate the JS glue.

## The wasm-bindgen version coupling

`wasm-bindgen` (the crate) and `wasm-bindgen-cli` (the binary that post-processes
the `.wasm`) **must share the exact same bindgen schema version**. A mismatch
fails the build with an opaque error:

```
rust Wasm file schema version: 0.2.X
   this binary schema version: 0.2.Y
```

The two versions come from two different places:
- **crate**: pinned in `Cargo.toml` → `wasm-bindgen = "=0.2.121"` (wasm32 target only)
- **CLI**: provided by the flake from nixpkgs (`flake.nix` → `wasm-bindgen-cli`)

To stop them drifting:
- dependabot is told to `ignore: wasm-bindgen*` (`.github/dependabot.yml`) so it
  can't bump the crate past the flake's CLI.
- The `book` job in `.github/workflows/rust.yml` builds the demo on every PR, so
  a mismatch is caught before merge (not only on the `pages.yml` deploy).

## How to bump wasm-bindgen

The CLI is the constrained side (we use whatever nixpkgs ships). Bump it first,
then match the crate to it:

```bash
# 1. Move nixpkgs (and the CLI it provides) forward
nix flake update nixpkgs

# 2. Read the CLI version nixpkgs now provides
nix develop --command wasm-bindgen --version    # e.g. "wasm-bindgen 0.2.121"

# 3. Pin the crate to that exact version in Cargo.toml
#    [target.'cfg(target_arch = "wasm32")'.dependencies]
#    wasm-bindgen = "=<that version>"

# 4. Verify the demo builds end to end
nix develop --command book/build-wasm-demo.sh
```

Commit `flake.lock` + `Cargo.toml` together. Update the version mentioned in the
comments in `Cargo.toml`, `flake.nix`, and this file.
