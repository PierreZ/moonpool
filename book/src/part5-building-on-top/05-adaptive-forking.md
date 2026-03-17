# Adaptive Forking

<!-- toc -->

Fixed-count splitting gives every splitpoint the same number of timelines. The three-level energy budget caps total exploration and redistributes unused energy. But there is still a missing piece: **how do we decide when a mark is productive and when it is barren?**

This is the job of the adaptive forking algorithm. Instead of spawning all timelines at once, it works in **batches**, checking coverage yield after each batch. Productive marks earn more batches. Barren marks are cut off early and their remaining energy flows back to the pool.

## The Batch Loop

When an assertion triggers a splitpoint, the adaptive explorer does not immediately spawn `max_timelines` children. It spawns a batch of `batch_size` children (say, 4), waits for them to finish, and asks: **did any of those children discover something new?**

The answer comes from the coverage bitmap. Before merging each child's bitmap into the explored map, the parent calls `has_new_bits()`. If at least one child in the batch set a bit that was not in the explored map, the batch is **productive**. If no child set any new bit, the batch is **barren**.

The critical detail: the parent checks `has_new_bits()` **before** calling `merge_from()`. If it merged first, the second child's "new" bits would be masked by the first child's already-merged bits. Checking before merging ensures we accurately detect whether any child in the batch found genuinely new coverage.

```text
Mark "retry_path":
  Batch 1: spawn 4 timelines -> 2 found new bits  (productive)
  Batch 2: spawn 4 timelines -> 1 found new bits  (productive)
  Batch 3: spawn 4 timelines -> 0 found new bits  (barren!)
    -> Return remaining per-mark energy to pool.
    -> Stop exploring this mark.

Mark "partition_heal":
  Batch 1: spawn 4 timelines -> 3 found new bits  (productive)
  Batch 2: spawn 4 timelines -> 2 found new bits  (productive)
  Batch 3: spawn 4 timelines -> 2 found new bits  (productive)
  ...continues until per-mark budget exhausted...
  ...draws from reallocation pool for more...
  Batch 8: spawn 4 timelines -> hits max_timelines -> stop
```

The algorithm is greedy and practical: keep investing in a mark as long as it finds new paths. Stop when it stops finding them. Redistribute the savings.

## The min_timelines Floor

There is a subtlety. A mark might appear barren after just one batch, but actually guard a rich code path that requires a few attempts to enter. If we cut it off after 4 timelines, we might miss bugs that the 8th or 12th timeline would have found.

The `min_timelines` parameter sets a floor: even if a mark looks barren from the start, the explorer will run at least `min_timelines` timelines before giving up. This prevents premature abandonment of marks that are slow starters.

How high should `min_timelines` be? It depends on the coverage bitmap saturation. In simulations with few assertion paths, the bitmap is sparse and `has_new_bits()` is a reliable signal. A `min_timelines` of one batch (4-8) is sufficient. In simulations with many paths (hundreds or thousands of distinct `assert_sometimes_each!` values), the bitmap starts to saturate. The signal becomes noisy. You need a higher floor (60-100+) to give marks a fair chance.

The concrete numbers from moonpool's own simulation tests:

- **Maze simulation** (moderate assertion count): `min_timelines: 100`, `max_timelines: 200`, `per_mark_energy: 1000`. Global energy 50K. About 5,700 timelines, 103 bugs found, 4 seconds.

- **Dungeon simulation** (many assertion paths, bitmap saturates quickly): `min_timelines: 800`, `max_timelines: 2000`, `per_mark_energy: 20000`. Global energy 2M. About 268K timelines, 943 bugs found. The high `min_timelines` is necessary because the 8192-bit coverage bitmap saturates with many unique bucket hashes.

## Barren Mark Detection

A mark is declared barren when an entire batch of children produces zero new coverage bits **and** the mark has already run at least `min_timelines` total timelines. When this happens:

1. The mark's remaining per-mark energy is atomically swapped to 0
2. That energy is added to the reallocation pool via `fetch_add`
3. The explorer stops spawning children for this mark

This is not a permanent decision. If the same assertion fires again at a deeper fork depth (in a child timeline), it gets a fresh per-mark budget. The barren classification applies to one invocation of the split loop, not to the assertion itself.

## Energy Flow in Practice

Let us trace the energy flow through a concrete example. A simulation has 5 splitpoints (marks A through E) with `per_mark_energy: 100` and `global_energy: 1000`.

```text
Initial state:
  Global: 1000
  Mark A: 100  Mark B: 100  Mark C: 100  Mark D: 100  Mark E: 100
  Pool: 0

Mark A triggers. Productive. Exhausts budget.
  Global: 900  Mark A: 0    ...    Pool: 0

Mark B triggers. Barren after 20 timelines. Returns 80.
  Global: 880  Mark B: 0    ...    Pool: 80

Mark C triggers. Productive. Exhausts budget. Draws 80 from pool.
  Global: 700  Mark C: 0    ...    Pool: 0

Mark D triggers. Barren after 10 timelines. Returns 90.
  Global: 690  Mark D: 0    ...    Pool: 90

Mark E triggers. Productive. Exhausts budget. Draws 90 from pool.
  Global: 500  Mark E: 0    ...    Pool: 0
```

Mark C and E each got 180 timelines (100 per-mark + 80 or 90 from pool), while marks B and D got only 20 and 10. The energy system automatically invested more in productive areas and less in unproductive ones, with zero manual tuning.

## Parallel Adaptive Forking

When `parallelism` is configured, the adaptive loop uses a **sliding window** of concurrent children instead of spawning one child at a time. The parent maintains a pool of bitmap slots (one per CPU core), and children write their coverage to their assigned slot. When a slot's child finishes (detected via `waitpid(-1)`), the parent merges that slot's bitmap, recycles the slot, and spawns the next child.

The batch yield check still happens at batch boundaries. The parent drains all active children before deciding whether to continue. This means parallelism speeds up each batch but does not change the stop/continue decision logic.

```text
Sequential (4 children):
  fork-wait-merge  fork-wait-merge  fork-wait-merge  fork-wait-merge
  [----batch----]  check yield

Parallel (4 children, 4 cores):
  fork  fork  fork  fork
  wait-any  merge  wait-any  merge  wait-any  merge  wait-any  merge
  [------------------batch------------------]  check yield
```

The parallel path achieves near-linear speedup for CPU-bound simulations. A 16-core machine processes 16 children concurrently, reducing a 1000-timeline exploration from minutes to seconds.

## Configuration Guide

```rust
AdaptiveConfig {
    batch_size: 4,           // children per batch before checking yield
    min_timelines: 60,       // minimum before declaring barren
    max_timelines: 200,      // hard cap even for productive marks
    per_mark_energy: 1000,   // initial budget per mark
    warm_min_timelines: None, // for multi-seed (next chapter)
}
```

Rules of thumb:

- **batch_size**: 4-8 is a good default. Smaller batches detect barren marks faster. Larger batches reduce the overhead of yield checks.
- **min_timelines**: Start low (batch_size). Increase if you see productive marks being cut off prematurely. If your simulation has many `assert_sometimes_each!` values, go higher (100+).
- **max_timelines**: Cap based on your time budget. Each timeline runs a full simulation.
- **per_mark_energy**: Start at 5-10x `min_timelines`. The surplus feeds the reallocation pool.

The adaptive system turns exploration from a "spray and pray" strategy into an intelligent resource allocator that automatically concentrates compute where it produces results. But even the best single-seed adaptive exploration eventually hits diminishing returns. In the next chapter, we will see how running multiple seeds breaks through that ceiling.
