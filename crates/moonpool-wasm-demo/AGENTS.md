# moonpool-wasm-demo

The run engine of the website: `run(config)` runs one seed of the small
system of the site (a replicated register, `src/register.rs`) and returns the
run as JSON (`Run` in `src/engine.rs`, versioned by `MODEL`). Every live run of
the site is this engine, compiled to wasm and loaded in a Web Worker. Built and
staged in `website/site/public/wasm-demo/pkg/` by
`website/site/scripts/build-wasm-demo.sh`: `cargo build --target
wasm32-unknown-unknown` then `wasm-bindgen` to generate the JS glue.

- `tests/site_runs.rs` records the runs of each page in
  `website/site/src/runs/`. A change to what a run does makes that test fail
  once and write the files again: commit them.
- `scripts/check-site-wasm.sh <seed>...` (repository root) proves that one
  config gives the same JSON natively (`--bin site-run`) and on wasm32 under
  node.
- `--bin sim-register` is the same system as an example simulation
  (`cargo xtask sim run register`, with `check_determinism`).
- The planted bug (`bug: true`) breaks the invariant on about 14% of seeds
  with the default chaos, and no seed fails with it off. Measure again with
  `SEEDS=2000 cargo test -p moonpool-wasm-demo --test engine -- --ignored`
  after a change to the scenario, and keep it between 5% and 20%.
- A change to a field of `Run` that the player reads
  (`website/site/public/run-model.js`) bumps `MODEL` and updates the player.

## The wasm-bindgen version coupling

`wasm-bindgen` (the crate) and `wasm-bindgen-cli` (the binary that post-processes
the `.wasm`) **must share the exact same bindgen schema version**. A mismatch
fails the build with an opaque error:

```
rust Wasm file schema version: 0.2.X
   this binary schema version: 0.2.Y
```

The two versions come from two different places:
- **crate**: pinned in `Cargo.toml` → `wasm-bindgen = "=0.2.129"` (wasm32 target only)
- **CLI**: provided by the flake from nixpkgs (`flake.nix` → `wasm-bindgen-cli`)

To stop them drifting:
- dependabot is told to `ignore: wasm-bindgen*` (`.github/dependabot.yml`) so it
  can't bump the crate past the flake's CLI.
- The `website` job in `.github/workflows/rust.yml` builds the demo on every PR, so
  a mismatch is caught before merge (not only on the `pages.yml` deploy).

## How to bump wasm-bindgen

The CLI is the constrained side (we use whatever nixpkgs ships). Bump it first,
then match the crate to it:

```bash
# 1. Move nixpkgs (and the CLI it provides) forward
nix flake update nixpkgs

# 2. Read the CLI version nixpkgs now provides
nix develop --command wasm-bindgen --version    # e.g. "wasm-bindgen 0.2.129"

# 3. Pin the crate to that exact version in Cargo.toml
#    [target.'cfg(target_arch = "wasm32")'.dependencies]
#    wasm-bindgen = "=<that version>"

# 4. Verify the demo builds end to end
nix develop --command website/site/scripts/build-wasm-demo.sh
```

Commit `flake.lock` + `Cargo.toml` together. Update the version mentioned in the
comments in `Cargo.toml`, `flake.nix`, and this file.
