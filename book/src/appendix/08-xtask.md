# Running Simulation Binaries with xtask

<!-- toc -->

Simulation binaries are long campaigns that do not belong in `cargo nextest`. The `xtask` crate runs them with LLVM sanitizer coverage so that `UntilCoverageStable` judges saturation on real code edges. `.cargo/config.toml` defines the alias: `xtask = "run --package xtask --"`.

## Commands

| Command | Effect |
|---------|--------|
| `cargo xtask sim list [filter...]` | Print the display names of the registered binaries, filtered by substring. |
| `cargo xtask sim run <filter...> [-- <args...>]` | Build and run every binary whose name contains a filter. Arguments after `--` go to the binary. |
| `cargo xtask sim run-all` | Run every registered binary in order. |

A filter matches the binary name as a substring. `cargo xtask sim run rpc` runs all seven RPC campaigns. Display names drop the `sim-` prefix, so `maze` and `sim-maze-explore` both match. Each binary prints its own report; xtask prints `--- name ---` around it, the elapsed time, and a summary of how many passed and failed. The command exits non-zero when any binary fails.

Inside the Nix shell the command is `nix develop --command cargo xtask sim run maze`.

## Registered Binaries

| Display name | Package | `SANCOV_CRATES` |
|--------------|---------|-----------------|
| `maze-explore` | `moonpool-sim-examples` | `moonpool_sim_examples` |
| `dungeon-explore` | `moonpool-sim-examples` | `moonpool_sim_examples` |
| `frontier-explore` | `moonpool-explorer` | `moonpool_explorer` |
| `axum-web` | `moonpool-sim-examples` | `moonpool_sim_examples` |
| `metrics-service` | `moonpool-sim-examples` | `moonpool_sim_examples` |
| `topology` | `moonpool-sim-examples` | `moonpool_sim_examples` |
| `tonic-grpc` | `moonpool-sim-examples` | `moonpool_sim_examples` |
| `journal` | `moonpool-sim-examples` | `moonpool_sim_examples,moonpool_journal` |
| `rpc-foundations`, `rpc-delivery`, `rpc-interfaces`, `rpc-balance`, `rpc-streams`, `rpc-security`, `rpc-qualification` | `moonpool-rpc-sim` | `moonpool_rpc,moonpool_rpc_sim` |

The list is the `SIM_BINARIES` constant in `crates/xtask/src/main.rs`.

## What xtask Runs

For each binary, xtask executes from the workspace root:

```text
SANCOV_CRATES=<crates> cargo run --package <pkg> --bin <name> --target-dir target/sancov [-- <args>]
```

Two details matter:

1. **A separate target directory.** `SANCOV_CRATES` is not part of cargo's fingerprint. Without `target/sancov`, cargo would serve a cached, uninstrumented build.
2. **The RUSTC_WRAPPER.** The Nix shell exports `RUSTC_WRAPPER=scripts/sancov-rustc.sh`. The wrapper reads `SANCOV_CRATES` and adds, for the listed crates only, `-Cpasses=sancov-module -Cllvm-args=-sanitizer-coverage-level=3 -Cllvm-args=-sanitizer-coverage-inline-8bit-counters -Ccodegen-units=1`. It never instruments build scripts or proc macros. With `SANCOV_CRATES` unset it passes through to rustc unchanged.

Only the crates in `SANCOV_CRATES` get counters. Instrument the crate under test and the library whose branches the campaign exercises (`moonpool_journal` for the journal campaign), not the whole framework. More instrumented edges make the plateau slower to reach.

## What Coverage Changes

With counters present, `moonpool_explorer::sancov_is_available()` is true and the runner uses `SaturationSignal::CodeCoverage`: edges covered per seed, cumulative, judged against `plateau_seeds`. Without counters it falls back to `SaturationSignal::AssertionCoverage`. The report shows which one it used, with `edges_covered` and `edges_total` in `report.saturation`. Under exploration, `ExplorationReport` carries `sancov_edges_total` and `sancov_edges_covered`, and `code_coverage_edges` merges every worker's counters into one history map. Coverage is a plateau signal only. It never anchors an exploration recipe.

Without the wrapper (plain `cargo run`), the binary still runs. It only loses the code-coverage signal.

## Adding a Binary

1. Add a `[[bin]]` named `sim-<name>` to the package, with its `main` calling `SimulationBuilder::run()` and `report.eprint()`.
2. Exit non-zero when `report.is_failure()` is true.
3. Add a `SimBinary` entry to `SIM_BINARIES` in `crates/xtask/src/main.rs`: `SimBinary::example("sim-<name>")` for `moonpool-sim-examples`, `SimBinary::rpc(..)` for `moonpool-rpc-sim`, or `SimBinary::new(name, package, sancov_crates)` otherwise.
4. Add the display name to the `sim` matrix in `.github/workflows/rust.yml`.
5. Run `cargo xtask sim list <name>` and `cargo xtask sim run <name>` once locally.

A binary that needs a flag (a seed, a mode) reads it from `std::env::args()` and gets it through `cargo xtask sim run <name> -- <flag>`.

## In CI

The `sim` job runs each registered binary as one matrix entry with `nix develop --command cargo xtask sim run <name>`, with `fail-fast: false`, and uploads the log on failure. The `macos` job runs `cargo xtask sim run frontier-explore` as well, because the explorer's `fork`, `waitpid` and `MAP_SHARED` usage and the Mach-O sancov sections must keep working on Darwin.
