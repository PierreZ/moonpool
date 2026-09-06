---
name: exploration-internals
description: Work on moonpool-explorer, the fork-based frontier exploration - controller, frontier, journal of discoveries, recipes as (rng_call_count, seed) breakpoints, exemplars, bounded worker pool, POSIX shared memory for the assertion region, sancov edge coverage via the xtask RUSTC_WRAPPER, and the exploration feature gate that keeps it out of wasm and the lean tree. Use when changing how discoveries fork, replay_timeline or BugRecipe, workers / branching / frontier limits, sancov instrumentation, or the frontier-explore scenarios.
---

# Exploration internals

`crates/moonpool-explorer` turns runs into accumulated knowledge: a timeline
that reaches a globally new interesting state is remembered as a **recipe**,
a controller schedules bounded continuations from it, and timelines that find
nothing die. It is libc/fork/mmap and is **never** built for wasm; moonpool-sim
pulls it in only behind the default-on `exploration` feature, and the
assertion accounting must keep working without it (the heap table).

## Vocabulary (from the crate doc, keep it consistent)

- **Seed**: a `u64` that determines a run. **Timeline**: a root seed plus a
  recipe. **Recipe**: `(rng_call_count, seed)` breakpoints: "run with the root
  seed; after N draws reseed to S". Replaying a recipe reproduces the
  timeline exactly, scheduler decisions included, because scheduling is a
  draw on the same stream (`sim/rng.rs`, `set_rng_breakpoints`).
- **Discovery**: the first pass of an `assert_sometimes!`/`assert_reachable!`,
  a new `assert_sometimes_each!` bucket, a partial `sometimes_all` truth
  combination, or a numeric/frontier/quality watermark improvement; guarded by
  atomic state in the shared assertion region so each transition is observed
  once across all timelines and seeds. **Journal**: one run's discoveries
  with their draw counts. **Frontier**: the FIFO of recipes to explore.
  **Exemplar**: a retained recipe and anchor per semantic state.

## Map

| File | Role |
|---|---|
| `controller.rs` | the controller and `ExplorationConfig` (`workers`, `max_runs_per_seed`, `branching_factor`, `max_frontier`, `max_recipe_len`): frontier, scheduling continuations, budgets |
| `journal.rs` | per-run discovery journal |
| `worker.rs` | the bounded pool of short-lived forked workers |
| `shared_mem.rs` | owned POSIX shared memory for the assertion region and cross-process state |
| `sancov.rs` | LLVM SanitizerCoverage `inline-8bit-counters` edge coverage, the signal `until_coverage_stable` plateaus on |
| `replay.rs` | recipe serialization; `SimulationBuilder::replay_timeline(seed, recipe)` and `BugRecipe` on the report |
| `simulations.rs`, `src/bin/sim/frontier_explore.rs` | the explorer's own scenarios (`sim-frontier-explore`) |
| `crates/moonpool-sim/src/chaos/exploration_glue.rs` | the sim side: installs the discovery hook, forks at discovery |

`workers: 0` explores in-process (one timeline at a time, no fork); paros runs
that way. Forked workers need every workload and process to be
factory-created, because a worker rebuilds the world from a fresh builder.

## Coverage

Under `cargo xtask sim run <name>` the binary is built with edge
instrumentation: the flake's `shellHook` exports `scripts/sancov-rustc.sh` as
`RUSTC_WRAPPER`, xtask sets `SANCOV_CRATES` to the crates (underscored) whose
edges count and builds into `target/sancov`, the wrapper adds
`-Cpasses=sancov-module` for exactly those crates, and `until_coverage_stable`
plateaus on real edges.
Under plain `cargo run` it falls back to assertion coverage. Any change to
the counters' layout or the region size touches `shared_mem.rs`,
`moonpool-assertions`' `ASSERTION_TABLE_MEM_SIZE`, and the frontier-explore
scenario.

## Rules

- A discovery must be **idempotent across processes**: the atomic guard in
  the shared region is the only thing that stops two workers from both
  claiming a first pass.
- Recipes are only valid while the draw schedule is stable; a runtime change
  invalidates every stored recipe, which is expected. Do not persist recipes
  across builds.
- Keep the resource bounds honest: the frontier-explore scenario validates
  that the physical footprint stays at one controller plus at most `workers`
  processes; `tests/exploration.rs` + `tests/exploration/` pin the rest.
- The macOS CI job runs `cargo xtask sim run frontier-explore`; fork and
  shared-memory changes must pass there too.

Book: `book/src/part5-building-on-top/*.md` (the exploration problem, the
frontier controller, bounded workers, exemplars and continuations, multi-seed,
exploring a consensus protocol).
