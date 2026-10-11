# moonpool-explorer

Frontier-based exploration for deterministic simulation testing.

## Why

One simulation seed follows one execution path. More independent seeds add
breadth, but a bug that needs a sequence of unlikely states stays hard to
reach: each seed starts again from the beginning. The explorer turns each
globally new assertion outcome into a replayable frontier job, then continues
that proven prefix with fresh deterministic randomness.

## How

When an `assert_sometimes!`, `assert_reachable!`, an `assert_sometimes_each!`
bucket, a partial truth combination or a numeric watermark makes globally new
progress, moonpool records the current RNG call count. The controller keeps a
recipe for that timeline and queues bounded continuations:

```text
root seed 42
  └─ discovers "leader changed" at RNG call 1000
       ├─ replay to 1000, then reseed to A
       ├─ replay to 1000, then reseed to B ── finds a bug
       └─ replay to 1000, then reseed to C
```

A recipe is a list of `(rng_call_count, seed)` breakpoints, printed as
`1000@A -> 1490@B`. Each worker starts the simulation from the beginning,
replays the recipe, and diverges after its last breakpoint. Deep states cost
a longer recipe, not a tree of live processes.

```text
controller (frontier, exemplars, statistics, bug recipes)
    ├─ worker: replay one recipe, report its journal, exit
    ├─ worker: replay one recipe, report its journal, exit
    └─ worker: replay one recipe, report its journal, exit
```

The controller keeps at most `workers` children alive. Workers never fork
again and never decide anything. With `workers: 0`, exploration runs
in-process: sequential, deterministic and fork-free. Discovery latches live
in `MAP_SHARED` memory (from `moonpool-assertions`), so each discovery counts
once across all workers and seeds.

## Usage

Configure exploration through `moonpool_sim::SimulationBuilder`:

```rust
use moonpool_sim::{ExplorationConfig, SimulationBuilder};

let report = SimulationBuilder::new()
    .enable_exploration(ExplorationConfig {
        workers: 0, // deterministic, fork-free starting point
        max_runs_per_seed: 8_000,
        branching_factor: 4,
        max_frontier: 1_024,
        max_recipe_len: 64,
    })
    .until_coverage_stable(10, 1_000)
    .workload_factory(|| Box::new(MyWorkload::new()))
    .run()?;
```

| Field | Default | Meaning |
|---|---|---|
| `workers` | 0 | Live worker processes. 0 runs every job in-process |
| `max_runs_per_seed` | 512 | Timelines per root seed. A ceiling, not a quota |
| `branching_factor` | 4 | Continuations queued when a run is productive |
| `max_frontier` | 1024 | Bound on queued jobs |
| `max_recipe_len` | 64 | Bound on replay depth, in segments |

Exploration needs factory-created workloads and built-in chaos surfaces, so
every continuation starts from state it can rebuild. A bug recipe in
`report.exploration` replays with
`SimulationBuilder::replay_timeline(seed, recipe)`.

## Features

- `Explorer`: the controller (frontier, exemplars, `ExplorationStats`, bug
  recipes, per-worker timeout `DEFAULT_WORKER_TIMEOUT`).
- `Recipe`, `format_timeline`, `parse_timeline`: replay breakpoints and their
  text form.
- `journal`: the discoveries one run made, stamped with the RNG call count.
- `init_assertions` / `cleanup_assertions`: the shared assertion region.
- `sancov`: LLVM `SanitizerCoverage` edge counters for reports and the
  `UntilCoverageStable` plateau. `cargo xtask sim run` builds the simulation
  binaries with this instrumentation. Frontier expansion stays
  assertion-guided, because only an assertion carries a replay anchor.
- `simulations`: a synthetic floor-ladder scenario that tests the full
  controller loop (`cargo xtask sim run frontier-explore`).

## Feature flags

None. The crate uses `libc` (`fork`, `waitpid`, `mmap`) and never builds for
`wasm32-unknown-unknown`. `moonpool-sim` depends on it behind its
`exploration` feature (on by default).

## Platform and scope

Worker processes need POSIX `fork` (Linux or macOS). Exploration is
assertion-guided randomized testing, not exhaustive model checking or a
formal proof. Reproducibility needs the system under test to use moonpool
providers, not external time, randomness, threads or I/O. Use worker
processes only from a standalone single-threaded simulation binary: raw
`fork()` is unsafe when other host threads may hold inherited locks.

## Documentation

- [Multiverse Exploration](https://pierrez.github.io/moonpool/part5-building-on-top/01-exploration.html) in the Sim Book
- [API documentation](https://docs.rs/moonpool-explorer)

## License

Apache 2.0
