# Multiverse Exploration

<!-- toc -->

Throughout this book, we have built a simulation framework that runs deterministic tests, injects chaos, and validates correctness with assertions. We can run thousands of seeds, each exploring a different corner of the state space. For many bugs, that is enough.

But some bugs are not found by running more seeds. They require a **sequence** of unlikely events, and no single seed happens to produce that exact sequence. We teased this in Part I when we described the vision of simulation-driven development. Now we deliver the capstone feature: **multiverse exploration**.

## The Core Idea

When a simulation reaches an interesting state for the first time (a retry fires, a timeout triggers, a leader election completes), we snapshot the entire universe and explore variations from that point. Instead of starting over with a new seed, we fork the process and continue from the discovery point with different randomness.

The insight is simple but powerful: if a seed managed to reach an interesting state, running forward from that state with different random choices is far more likely to find a second interesting event than starting from scratch. We are investing our exploration budget where it has already proven productive.

This is what Antithesis calls **checkpoint-and-branch**. In moonpool, we call it **multiverse exploration** because it creates a tree of alternate timelines branching from key discovery points.

## Vocabulary

Before we go further, let us establish the terms we will use throughout this section.

**Seed** is a `u64` that completely determines a simulation's randomness. Same seed means same coin flips means same execution, every time. This is the foundation from Part II.

**Timeline** is one complete simulation run. A root seed plus a sequence of reseeding points uniquely identifies a timeline.

**Splitpoint** is a moment where the explorer decides to branch. It happens when a `sometimes` assertion succeeds for the first time, a numeric watermark improves, or a frontier advances. The splitpoint is identified by the RNG call count at the moment of discovery.

**Multiverse** is the tree of all timelines explored from one root seed. Each splitpoint creates new children with different seeds, and those children can encounter their own splitpoints, creating a recursive tree.

**Recipe** is the path from the root timeline to a specific descendant. It is a sequence of `(rng_call_count, child_seed)` pairs that tells you exactly which forks to take. If a bug is found ten levels deep in the multiverse tree, the recipe tells you how to get back there:

```text
151@8837201 -> 80@1293847 -> 42@9918273
```

That reads: "at RNG call #151, reseed to 8837201. At call #80 in the new timeline, reseed to 1293847. At call #42, reseed to 9918273." Follow those instructions and you arrive at the exact same bug, deterministically.

**Mark** is an assertion site that can trigger splitpoints. Each mark has a name, a shared-memory slot, and in adaptive mode its own energy allowance.

## What This Section Covers

Over the next five chapters, we will build up the complete exploration system piece by piece:

1. **The Exploration Problem** explains why random simulation is not enough for hard bugs, and frames exploration as a resource allocation problem using NES game analogies from Antithesis.

2. **Fork at Discovery** describes the mechanism: OS-level `fork()`, shared memory, coverage bitmaps, and bug recipes.

3. **Coverage and Energy Budgets** introduces energy as a finite exploration resource that prevents unbounded forking.

4. **Adaptive Forking** replaces fixed-count splitting with batch-based forking that automatically invests more in productive splitpoints.

5. **Multi-Seed Exploration** shows how running multiple root seeds with moderate energy finds more bugs than a single seed with massive energy.

Each chapter builds on the previous one. The concepts are layered: first the problem, then the basic mechanism, then increasingly sophisticated resource management. By the end, you will understand how moonpool turns a single simulation seed into a tree of thousands of timelines that systematically hunt for bugs hiding behind sequences of unlikely events.

The `moonpool-explorer` crate that implements all of this is a leaf dependency with exactly one external dependency: `libc`. It has zero knowledge of processes, networks, or storage. It communicates with the simulation through two function pointers: one to read the RNG call count, and one to reseed the RNG. That minimal coupling is deliberate. The exploration engine is a general-purpose tool that works with any deterministic simulation, not just moonpool's.

Let us start with the problem it solves.
