# Coverage and Energy Budgets

<!-- toc -->

The fork-at-discovery mechanism is powerful, but it has an obvious failure mode: exponential blowup. A splitpoint at depth 0 forks 4 children. Each of those children might hit a different splitpoint and fork 4 more. Two levels deep, we have 16 timelines. Three levels, 64. A simulation with many assertion sites and deep nesting could spawn millions of processes, exhausting memory and CPU.

We need a budget. Something that caps the total work and distributes it intelligently. In moonpool, that budget is called **energy**.

## The Simple Model: Fixed-Count Splitting

The simplest exploration mode allocates a flat global energy pool. Every timeline spawned costs one unit. When energy hits zero, no more timelines are created, regardless of how many splitpoints remain untriggered.

```rust
ExplorationConfig {
    max_depth: 2,
    timelines_per_split: 3,
    global_energy: 50,
    adaptive: None,
    parallelism: None,
}
```

This says: fork up to 3 timelines at each splitpoint, allow nesting up to depth 2, and spend at most 50 total timelines across the entire exploration. If 17 splitpoints trigger, the first 16 get their 3 timelines each (48 total), and the 17th gets 2 before energy runs out.

Fixed-count mode is easy to reason about. The maximum wall-clock time is bounded: 50 timelines, each running a full simulation. For quick smoke tests and CI, this is often all you need.

But it has a problem. Every splitpoint gets the same budget (3 timelines), regardless of whether it is productive (finding new code paths) or barren (repeating paths already seen). A splitpoint guarding a dead-end code path gets the same investment as one guarding a rich, unexplored subtree. That is wasteful.

## The Three-Level Energy Budget

Adaptive mode replaces the flat pool with a three-level energy system. Each level serves a different purpose.

### Level 1: Global Energy

The hard cap. Every timeline, regardless of which splitpoint created it, consumes one unit of global energy. When global energy reaches zero, **all** exploration stops. This is the circuit breaker that prevents runaway forking.

### Level 2: Per-Mark Energy

Each assertion mark (splitpoint) gets its own initial budget. When a mark first triggers, it is allocated `per_mark_energy` units. The mark can only spawn timelines if it has per-mark energy remaining. This ensures that no single mark can consume the entire global budget.

### Level 3: The Reallocation Pool

Here is where it gets interesting. When a mark is declared **barren** (it has spawned several batches of timelines and none of them found new coverage bits), its remaining per-mark energy is returned to a shared **reallocation pool**. Productive marks that exhaust their initial per-mark budget can draw from this pool to keep exploring.

Energy flows downhill: from barren marks, through the reallocation pool, to productive marks.

```text
+------------------------------------------------------------+
|                  GLOBAL ENERGY (100K)                       |
|  Every timeline consumes 1 from here first.                |
|  When this hits 0, ALL exploration stops.                  |
+------------------------------------------------------------+
|                                                            |
|  +-----------------+  +-----------------+  +------------+  |
|  |  Mark A: 1000   |  |  Mark B: 1000   |  | Mark C:    |  |
|  |  (productive)   |  |  (barren)       |  | 1000 (new) |  |
|  |  Used: 1000     |  |  Used: 60       |  | Used: 0    |  |
|  |  Needs more! ---+--+--- Returns 940 -+--+            |  |
|  +--------+--------+  +-----------------+  +------------+  |
|           |                     |                          |
|           v                     v                          |
|  +----------------------------------------------------+   |
|  |           REALLOCATION POOL: 940                    |   |
|  |  Energy returned by barren marks.                   |   |
|  |  Productive marks draw from here when their         |   |
|  |  per-mark budget runs out.                          |   |
|  +----------------------------------------------------+   |
+------------------------------------------------------------+
```

### The Decrement Sequence

When the explorer wants to spawn one timeline for a mark, it follows a strict sequence:

1. **Decrement global energy by 1.** If global is at 0, stop. The hard cap is absolute.

2. **Decrement per-mark energy by 1.** If the mark still has budget, proceed.

3. **If per-mark is exhausted, try the reallocation pool.** Decrement pool by 1. If the pool has energy, proceed.

4. **If neither per-mark nor pool has energy, undo the global decrement and stop for this mark.** The mark is out of resources, but global energy is preserved for other marks.

All of these decrements use atomic `fetch_sub` operations in shared memory, with rollback on failure. This ensures consistency even when nested children (at different fork depths) are competing for the same budget.

## The Coverage Bitmap and Saturation

The 8192-bit coverage bitmap is what the explorer uses to decide whether a mark is productive or barren. After each child finishes, the parent checks: did this child set any bits that the explored map did not already have?

But 8192 bits is a small space. With many assertion sites, hash collisions are inevitable. And as exploration progresses, more bits get set. Eventually, the bitmap **saturates**: most or all bits are set, and every new child appears to contribute "nothing new" even if it explored genuinely different behavior.

This is a real issue. A simulation with 200 assertion sites, each hashing to a different bit, fills 200 of 8192 positions. That is only 2.4% saturation, which is fine. But `assert_sometimes_each!` assertions with many identity values can hash hundreds of distinct values into the bitmap. A simulation tracking 1000 partition keys sets 1000 bits, reaching 12% saturation. At higher counts, collisions make the `has_new_bits()` check less discriminating.

The practical consequence: simulations with many unique assertion paths need higher `min_timelines` (the minimum exploration per mark before declaring it barren) because the coverage bitmap loses resolution. We will see how the adaptive forking system handles this in the next chapter.

For simulations with moderate assertion counts (under a few hundred unique paths), the 8192-bit bitmap is more than sufficient. The signal is clear, barren marks are detected quickly, and energy flows efficiently from unproductive to productive splitpoints.

## Configuration

```rust
ExplorationConfig {
    max_depth: 3,
    timelines_per_split: 4,  // ignored in adaptive mode
    global_energy: 200_000,
    adaptive: Some(AdaptiveConfig {
        batch_size: 4,
        min_timelines: 4,
        max_timelines: 200,
        per_mark_energy: 1000,
        warm_min_timelines: None,
    }),
    parallelism: Some(Parallelism::MaxCores),
}
```

The `max_depth` parameter limits how deep the fork tree can grow. A depth of 3 means the root can fork children (depth 1), those children can fork grandchildren (depth 2), and grandchildren can fork great-grandchildren (depth 3). Beyond that, splitpoints are ignored. This prevents combinatorial explosion in fork tree depth while still allowing multi-step bugs to be discovered.

`global_energy` is the total number of timelines across the entire exploration. `per_mark_energy` is the initial budget per splitpoint. `max_timelines` is a hard cap per mark, even if it is still productive. The relationship between these values controls the exploration strategy:

- **High global, low per-mark**: many marks get explored, but none deeply. Good for breadth-dominated problems (the Zelda shape).
- **High global, high per-mark**: fewer marks, but each explored thoroughly. Good for depth-dominated problems (the Gradius shape).
- **Large reallocation pool** (from many barren marks): self-correcting. Budget flows to where it is productive.

The energy system is the bridge between "fork whenever something interesting happens" and "do not melt the computer." It makes exploration a disciplined, bounded operation that can run in CI with predictable resource consumption. In the next chapter, we will see the adaptive algorithm that decides **when** a mark is barren and **how** to redistribute its energy.
