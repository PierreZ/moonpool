# Multi-Seed Exploration

<!-- toc -->

Even with adaptive forking and energy budgets, running a single root seed with a massive energy budget eventually hits diminishing returns. The coverage bitmap saturates. Every mark looks barren because every new child's bits overlap with the already-dense explored map. Productive marks get cut off too early, and energy piles up in the reallocation pool with nowhere useful to go.

The fundamental issue is that a single seed reaches a particular region of the state space. No amount of branching can explore regions that the root timeline's initial execution path never touches. If the first 100 RNG calls establish a specific network topology and failure pattern, all child timelines share that foundation. They explore variations on a theme, not genuinely different themes.

The solution: **run multiple root seeds, each with moderate energy, preserving coverage information across seeds.**

## Coverage-Preserving Seed Transitions

The naive approach to multi-seed exploration would be to reset everything between seeds: zero the explored map, reset all assertion state, start fresh. But that throws away valuable information. If seed 1 discovered that `retry_path` is productive, seed 2 should know that.

Moonpool's `prepare_next_seed()` function performs a **selective reset** that preserves cumulative knowledge while clearing per-seed transient state.

**Preserved across seeds:**
- The explored map (coverage bitmap union). Bits set by prior seeds stay set. The explored map is the collective memory of the multiverse.
- Assertion watermarks and frontiers. If seed 1 achieved a numeric watermark of 42, seed 2's assertions know that 42 is the bar to beat. Only genuine improvements trigger new splitpoints.
- Best scores for bucketed assertions. Prior quality context carries forward.

**Reset between seeds:**
- Split triggers. Every mark can trigger fresh splitpoints with the new seed. A mark that was barren under seed 1 might be productive under seed 2 because the new seed reaches it through a different execution path.
- Pass/fail counters. Per-seed statistics start fresh.
- Energy budget. Each seed gets its own full energy allocation.
- Coverage bitmap (per-timeline). Cleared so each new root timeline starts with a clean slate.
- Bug recipe. Each seed can capture its own bug.

The preserved explored map is the key. It means that the coverage check `has_new_bits()` considers bits from **all** prior seeds. A child in seed 3 is only considered productive if it finds something that neither seed 1, seed 2, nor any of their children have seen. The bar rises progressively, focusing each seed's energy on genuinely unexplored territory.

## Warm Starts

There is a subtlety with preserved explored maps. When seed 2 starts, the explored map already has bits set from seed 1's exploration. Marks that were productive under seed 1 (and filled in many bits) will appear to seed 2 as already-explored. The adaptive loop might classify them as barren after just `min_timelines` attempts, even though seed 2 has a genuinely different execution path that could find new things.

The `warm_min_timelines` parameter addresses this. On warm starts (seeds after the first), marks that appear barren exit after `warm_min_timelines` instead of the full `min_timelines`. This is typically set lower than `min_timelines` because the explored map has prior context. There is less need for a long ramp-up.

```rust
AdaptiveConfig {
    batch_size: 20,
    min_timelines: 400,
    max_timelines: 2000,
    per_mark_energy: 10_000,
    warm_min_timelines: Some(30),  // warm seeds: 30 instead of 400
}
```

On seed 1, each mark gets at least 400 timelines before being declared barren. On seeds 2 and 3, marks that tread already-explored ground (which is most of them, since the explored map is dense) are cut off after 30 timelines. But marks that find genuinely new paths on the new seed ramp up to the full budget. Energy automatically flows to the marks where the new seed has something unique to contribute.

## Real Numbers

Here are actual results from moonpool's simulation tests:

**Dungeon simulation:**
- Single seed, 2M energy: ~35 seconds, found 943 bugs
- 3 seeds x 400K energy (1.2M total): ~24 seconds, found comparable bugs

Less total energy, faster wall-clock time, comparable bug discovery. The multi-seed run is faster because each seed explores a different region efficiently. The single massive seed spends most of its late-stage energy on timelines that find nothing new.

**Maze simulation:**
- Single seed, 50K energy: ~4 seconds
- 2 seeds x 20K energy (40K total): ~0.75 seconds

Five times faster with 20% less total energy. The two seeds happen to cover the state space from different angles, and each seed's warm start avoids re-exploring the other's territory.

These speedups come from the coverage-preserving reset. Without it, multi-seed exploration would be N independent runs with no shared knowledge, which is what plain `UntilAllSometimesReached` already does. The explored map is the innovation that makes multi-seed better than the sum of its parts.

## The Builder Loop

The simulation builder orchestrates multi-seed exploration as a loop:

```text
Seed 1 (cold start):
  - init() with full energy
  - run simulation
  - collect stats
  - cleanup exploration state

Seed 2 (warm start):
  - prepare_next_seed(energy)  // selective reset
  - skip_next_assertion_reset() // don't zero watermarks
  - run simulation
  - accumulate stats
  - cleanup exploration state

Seed 3 (warm start):
  - prepare_next_seed(energy)
  - skip_next_assertion_reset()
  - run simulation
  - accumulate stats
  - final cleanup
```

Each iteration after the first calls `prepare_next_seed()` instead of a full `init()`/`cleanup()` cycle. The `skip_next_assertion_reset()` function tells the simulation world not to zero the assertion table when it creates a new simulation, preserving the watermarks and frontiers that `prepare_next_seed()` carefully kept.

Statistics are accumulated across seeds. The final report shows the totals: total timelines, total bugs, total coverage bits, across all seeds.

## When to Use Multi-Seed

Multi-seed exploration is most valuable when:

- **The simulation has many assertion paths.** More assertions means faster bitmap saturation, which means each seed hits diminishing returns sooner. Multiple seeds keep the signal fresh.

- **You have a time budget.** 3 seeds at 400K energy with parallel forking finishes faster than 1 seed at 2M energy because warm seeds cut off re-explored territory quickly.

- **You want diversity.** Different root seeds produce different initial topologies, failure patterns, and timing. Each seed explores a region that the others might never reach.

Multi-seed is less important when:

- **The simulation has few assertion paths.** With a sparse bitmap, the adaptive loop's barren detection works well and a single seed with enough energy explores efficiently.

- **You want maximum depth.** A single seed with massive energy explores the deepest possible fork trees. Multi-seed sacrifices per-seed depth for breadth across seeds.

## The Complete Picture

Let us step back and see how all the exploration pieces fit together.

**The problem**: bugs that require sequences of unlikely events are exponentially hard to find with random seeds.

**Fork at discovery**: when something interesting happens, snapshot the universe and explore variations. Uses `fork()`, shared memory, and coverage bitmaps. Reduces sequential-luck probability from multiplicative to additive.

**Energy budgets**: three-level system (global, per-mark, reallocation pool) that caps total work and redistributes resources from barren to productive marks.

**Adaptive forking**: batch-based exploration that detects barren marks via coverage yield and stops early. Productive marks get more investment automatically.

**Multi-seed exploration**: multiple root seeds with moderate energy, preserving coverage knowledge across seeds. Explores genuinely different state-space regions while avoiding redundant work.

Together, these form a system that automatically allocates exploration budget where it produces results, across multiple seeds, across many splitpoints, down to the individual batch level. The developer's job is to write good assertions (the "what to explore" specification) and set reasonable energy budgets (the "how much to spend" constraint). The exploration engine handles the rest.

The `moonpool-explorer` crate that implements all of this is 14 source files with a single external dependency (`libc`). It has zero knowledge of actors, networks, or storage. It communicates with the simulation through two function pointers. And it can turn a single-seed simulation run into a multiverse of thousands of timelines that systematically hunt for the bugs hiding behind layers of sequential luck.
