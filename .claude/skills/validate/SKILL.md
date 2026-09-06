---
name: validate
description: Run moonpool's full local validation gate (cargo fmt, clippy pedantic with -D warnings, nextest, the no-default-features and wasm32 portability checks, mdbook when the book changed) inside the Nix shell and fix what fails. Use before declaring any change done, before committing, when asked "does it pass CI", and after touching crate features or Cargo.toml.
---

# Validate

CI (`.github/workflows/rust.yml`) runs everything under `nix develop --command`.
Running the same commands locally is the only way to know a change is green
before pushing; the sandbox's own `cargo` is a different toolchain and its
result means nothing here.

## The gate, in order

Run each step and stop at the first failure. Fix, then rerun from the top so a
fix in one step cannot silently break an earlier one.

```bash
nix develop --command cargo fmt
nix develop --command cargo clippy -- --deny warnings
nix develop --command cargo nextest run
```

Portability, whenever a change touches crate structure, features, a
`Cargo.toml`, or anything under `moonpool-core`, `moonpool-sim`,
`moonpool-assertions` or `moonpool-buggify`:

```bash
nix develop --command cargo clippy -p moonpool-sim --no-default-features --all-targets -- --deny warnings
nix develop --command cargo nextest run -p moonpool-sim --no-default-features
nix develop --command cargo nextest run -p moonpool-core --features select
nix develop --command cargo check --target wasm32-unknown-unknown -p moonpool-assertions
nix develop --command cargo check --target wasm32-unknown-unknown -p moonpool-sim --no-default-features
nix develop --command cargo check --target wasm32-unknown-unknown -p moonpool-wasm-demo
```

CI also asserts that the lean production tree (`moonpool` with
`--no-default-features --features tokio`) pulls in none of sim, explorer, hyper
or prometheus. If you added a dependency edge, check it with
`cargo tree -p moonpool --no-default-features --features tokio -e no-dev`.

Simulation binaries, when a change touches the runner, the executor, the
network or storage engines, or one of the examples:

```bash
nix develop --command cargo xtask sim run <name>     # one of the registered binaries
nix develop --command cargo xtask sim run-all
```

The book, when a public API or documented behavior changed:

```bash
nix develop --command mdbook build book/
```

## Rules that decide how to fix a failure

- **Clippy pedantic is on and `#[allow(clippy::...)]` is never the fix.** A
  warning is telling you the function is too long, the type too wide or the
  name misleading; refactor. An allow attribute hides the signal and the next
  reader inherits the debt.
- **A failing assertion is a bug, not a flaky test.** Never delete or weaken an
  `assert_always!`/`assert_sometimes!` to get green. Stop, read the trace, find
  the root cause (`/debug-a-seed`).
- **`unwrap()` is not accepted.** Return `Result` with `?`; for lock poisoning
  use `.expect("RwLock poisoned: prior task panicked")`.
- **Every public item is documented.** Missing docs fail clippy in this
  workspace.

## Remote sessions

If `CLAUDE_CODE_REMOTE=true`, Nix is not preinstalled. Install it first
(`sudo apt-get install -y nix-bin`, then write
`experimental-features = nix-command flakes` to `~/.config/nix/nix.conf`) as
the root `AGENTS.md` describes. Do not fall back to the sandbox toolchain.
