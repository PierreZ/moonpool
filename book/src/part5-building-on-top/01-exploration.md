# Multiverse Exploration

<!-- toc -->

Throughout this book, we have built a simulation framework that runs deterministic tests, injects chaos, and validates correctness with assertions. We can run thousands of seeds, each exploring a different corner of the state space. For many bugs, that is enough.

But some bugs are not found by running more seeds. They require a **sequence** of unlikely events, and no single seed happens to produce that exact sequence. We teased this in Part I when we described the vision of simulation-driven development. Now we deliver the capstone feature: **multiverse exploration**.

## The Core Idea

Because the simulation is deterministic, an execution is not a fleeting thing — it is a *reproducible coordinate* in the state space. When a timeline reaches an interesting state for the first time (a retry fires, a leader election completes, the dungeon's next floor is entered), moonpool remembers *how to get back there*: the seed plus the exact RNG position of the discovery. Later, it replays that prefix and continues with fresh randomness — a short randomized rollout from a proven starting point.

The insight is simple but powerful: if a timeline managed to reach an interesting state, running forward from that state with different random choices is far more likely to find the next interesting event than starting from scratch. Exploration turns deterministic executions into **accumulated knowledge**: interesting states are remembered, resumed, and extended; barren branches simply die.

Crucially, the tree of explored timelines is a **logical** structure, not a physical one. The logical multiverse may contain thousands of timelines; the physical footprint is one controller process plus a small, fixed pool of short-lived workers. Reaching a state ten discoveries deep costs a recipe with ten segments — not a tree of live processes.

## Vocabulary

**Seed** is a `u64` that completely determines a simulation's randomness. Same seed means same coin flips means same execution, every time. This is the foundation from Part II.

**Timeline** is one complete simulation run. A root seed plus a recipe uniquely identifies a timeline.

**Recipe** is the replay path to a timeline: a sequence of `(rng_call_count, seed)` breakpoints. "Run with the root seed; after 151 RNG calls, reseed to 8837201; after 80 more, reseed to 1293847":

```text
151@8837201 -> 80@1293847 -> 42@9918273
```

Follow those instructions and you arrive at the exact same timeline, deterministically — task scheduling included, because the executor draws its schedule from the same counted stream the recipe replays.

**Discovery** is a globally-new interesting state: the first pass of an `assert_sometimes!`, a new `assert_sometimes_each!` bucket, a watermark or frontier improvement. Discoveries are latched atomically in shared memory, so each fires exactly once across all timelines.

**Journal** is the list of discoveries one run made, each stamped with the RNG call count where it happened — the coordinate a recipe breakpoint can anchor to.

**Frontier** is the queue of jobs (recipes) waiting to be explored.

**Exemplar** is a retained recipe that reaches one semantic state. The controller keeps a few per state, because two executions can hit the same state with very different future potential.

**Worker** is a forked process that executes exactly one timeline and exits. Workers never fork and never make exploration decisions.

## What This Section Covers

1. **The Exploration Problem** explains why random simulation is not enough for hard bugs, and frames exploration as a resource allocation problem using NES game analogies from Antithesis.

2. **The Frontier Controller** describes the exploration loop: recipes, discovery journals, the one-expansion rule, and how barren branches die without any budget accounting.

3. **Bounded Workers** covers the physical execution model: `fork()` as a bounded job-execution optimization rather than a tree structure, single-owner novelty, crash isolation, and macOS portability.

4. **Exemplars and Continuations** shows how the controller remembers semantic states, retains multiple exemplars per state, and schedules continuations from the under-explored frontier — the machinery that carries the dungeon workload to floor 8.

5. **Multi-Seed Exploration** shows how cumulative novelty makes additional root seeds cheap: seeds that discover nothing new stop after one run.

The `moonpool-explorer` crate that implements all of this is a leaf dependency with exactly one external dependency: `libc`. It has zero knowledge of processes, networks, or storage. It communicates with the simulation through the assertion accounting hooks and one function pointer that reads the RNG call count. That minimal coupling is deliberate: the exploration engine is a general-purpose controller that works with any deterministic simulation, not just moonpool's.

Let us start with the problem it solves.
