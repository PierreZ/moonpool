# moonpool-explorer

Frontier-based exploration for deterministic simulation testing.

## Why: Explore Beyond Independent Seeds

A single simulation seed follows one execution path. Independent seeds add
breadth, but bugs that require a sequence of unlikely states remain difficult
to reach. The explorer turns globally new assertion outcomes into replayable
frontier jobs, then continues those proven prefixes with fresh deterministic
randomness.

## How: Replay and Continue

When an `assert_sometimes!`, `assert_reachable!`, bucket, or guidance watermark
makes globally new progress, Moonpool records the current RNG call count. The
controller retains a recipe for that timeline and enqueues bounded
continuations:

```text
root seed 42
  └─ discovers "leader changed" at RNG call 1000
       ├─ replay to 1000, then reseed to A
       ├─ replay to 1000, then reseed to B ── finds a bug
       └─ replay to 1000, then reseed to C
```

Each worker starts the simulation from the beginning, replays the recipe, and
diverges after its last breakpoint. Forking is only a bounded process-execution
optimization between runs; workers do not fork recursively and the explorer
does not snapshot a live simulation.

## Architecture

```text
controller (frontier, exemplars, statistics)
    ├─ worker: replay one recipe, report journal, exit
    ├─ worker: replay one recipe, report journal, exit
    └─ worker: replay one recipe, report journal, exit
```

The controller keeps at most `workers` children live. Set `workers` to `0` for
fork-free, sequential exploration. The `moonpool-explorer` crate stays unaware
of Moonpool's network, storage, and process internals; `moonpool-sim` installs
the RNG and assertion hooks that connect them.

## Configuration

Most users should configure exploration through
`moonpool_sim::SimulationBuilder`:

```rust,ignore
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
    .run();
```

- `max_runs_per_seed` is a ceiling, not a quota. A root run that discovers
  nothing new ends after one timeline.
- `branching_factor` controls how many continuations a productive run creates.
- `max_frontier` bounds queued work, and `max_recipe_len` bounds replay depth.
- The physical process bound is one controller plus `workers` children.
- Factory-created workloads and built-in chaos surfaces are required so every
  continuation starts from reconstructible state.

Bug recipes in `report.exploration` can be replayed with
`SimulationBuilder::replay_timeline(seed, recipe)` when every timeline starts
from fresh deterministic test-driver state.

## Platform and Scope

Worker processes require POSIX `fork` support (Linux or macOS). Non-Unix
targets use in-process exploration. Exploration is assertion-guided randomized
testing, not exhaustive model checking or a formal proof. Reproducibility also
requires the system under test to use Moonpool providers rather than external
time, randomness, threads, or I/O. Use worker processes only from a standalone
single-threaded simulation binary; raw `fork()` is unsafe when unrelated host
threads may hold inherited locks.

## Documentation

- [Moonpool book](https://pierrezemb.github.io/moonpool/)
- [API documentation](https://docs.rs/moonpool-explorer)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
