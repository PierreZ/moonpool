# Multiverse Exploration

<!-- toc -->

A simulation campaign runs many seeds. Each seed is one timeline. For most bugs, many seeds are enough. Some bugs are different. They need a **sequence** of rare events, and no single seed produces that exact sequence. Multiverse exploration is the answer. It is the capstone of moonpool, and the `moonpool-explorer` crate implements it.

## The Core Idea

A deterministic simulation makes every execution a **reproducible coordinate**. When a timeline reaches an interesting state for the first time (a retry fires, a leader changes, a dungeon floor is entered), moonpool remembers how to get back there. The memory is small: the root seed plus the RNG call count of the discovery. Later, the explorer replays that prefix and continues with fresh randomness. This is a short randomized rollout from a proven starting point.

A timeline that reached an interesting state is a good place to start the search for the next one. Exploration turns executions into **accumulated knowledge**: interesting states are remembered, resumed and extended. Barren branches die.

The tree of explored timelines is a **logical** structure, not a physical one. The multiverse can hold thousands of timelines. The physical footprint is one controller process plus a small, fixed pool of short-lived workers. A state ten discoveries deep costs a recipe with ten segments, not ten live processes.

## Vocabulary

The explorer's own glossary, in `crates/moonpool-explorer/src/lib.rs`, uses these terms. The book uses the same ones.

| Term | Meaning |
|------|---------|
| **Seed** | A `u64` that fully determines a simulation's randomness. |
| **Timeline** | One complete simulation run: a root seed plus a recipe. |
| **Recipe** | Replay breakpoints: a list of `(rng_call_count, seed)` segments. |
| **Discovery** | A globally new interesting state, reported by an assertion macro. |
| **Journal** | The discoveries one run made, each stamped with its RNG call count. |
| **Frontier** | The FIFO queue of jobs (recipes) that wait to be explored. |
| **Exemplar** | A retained recipe plus anchor that reaches one semantic state. |
| **Worker** | A forked process that runs exactly one timeline and exits. |

A recipe reads as a one-line timeline string:

```text
151@8837201 -> 80@1293847 -> 42@9918273
```

"Run with the root seed. After 151 RNG calls, reseed to 8837201. After 80 more, reseed to 1293847. After 42 more, reseed to 9918273." Follow those instructions and you get the same timeline, task scheduling included. The executor draws its schedule from the same counted stream the recipe replays. `moonpool_sim::format_timeline` and `parse_timeline` convert between the string and the `Vec<(u64, u64)>`.

## Turning It On

Exploration is a builder option. It needs factory workloads, because every continuation starts a fresh test driver:

```rust,ignore
use moonpool_sim::{ExplorationConfig, SimulationBuilder};

let report = SimulationBuilder::new()
    .workload_factory(|| Box::new(MazeWorkload::default()))
    .enable_exploration(ExplorationConfig {
        workers: 0,              // in-process: deterministic, no fork
        max_runs_per_seed: 2400, // timelines per root seed, a ceiling
        branching_factor: 4,     // children per productive run
        max_frontier: 512,       // queued jobs
        max_recipe_len: 32,      // depth in replay segments
    })
    .set_iterations(2)
    .run()?;
```

This is the `sim-maze-explore` binary in `moonpool-sim-examples`. The `exploration` feature of `moonpool-sim` is on by default. It pulls in `moonpool-explorer`, which uses `libc` for `fork`, `waitpid` and `mmap`. Turn the feature off for a wasm build. Without it, the assertion macros still count in a heap table, and `UntilCoverageStable` still works.

## What This Part Covers

1. [The Exploration Problem](./02-exploration-problem.md) explains why random seeds are not enough for deep bugs, with the NES games Antithesis used as examples.
2. [The Frontier Controller](./03-frontier-controller.md) describes the loop: recipes, discovery journals, the one-expansion rule, and how barren branches die.
3. [Bounded Workers](./04-bounded-workers.md) covers the process model: `fork()` as a job executor, single-owner novelty, crash isolation, the wall-clock ceiling, and macOS portability.
4. [Exemplars and Continuations](./05-exemplars-continuations.md) shows how the controller remembers semantic states and re-invests in the under-explored frontier.
5. [Multi-Seed Exploration](./06-multi-seed.md) shows how cumulative novelty makes extra root seeds cheap.
6. [Exploring a Consensus Protocol](./07-exploring-consensus.md) applies all of it to Paxos-style systems.

`moonpool-explorer` is a leaf crate with one external dependency, `libc`. It knows nothing about processes, networks or storage. It talks to the simulation through the discovery hook of `moonpool-assertions` and one function pointer that reads the RNG call count (`set_rng_count_hook`). The coupling is minimal on purpose: the controller works with any deterministic simulation that counts its draws.
