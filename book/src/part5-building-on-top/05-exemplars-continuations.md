# Exemplars and Continuations

<!-- toc -->

Expansion alone is momentum: a productive run spawns a few children, and if a child is productive the wave carries forward. But waves die. Four children from the "key found on floor 5" moment might all get eaten by monsters, and with plain expansion that hard-won state would be lost. This chapter covers the machinery that *remembers* progress and keeps re-investing in it.

## Semantic States

Every discovery carries a **state id**: the assertion message hash for ordinary slot assertions, the site-plus-truth-combination hash for partial `assert_sometimes_all!` states, or the bucket hash (message plus identity keys) for `assert_sometimes_each!`. This is the Castlevania lesson from the problem chapter, applied through the macros you already write: the workload's assertions project the huge concrete state space into a tractable set of semantic buckets.

```rust,ignore
assert_sometimes_each!("descended",
    [("to_floor", floor)],       // identity → one state per floor
    [("health", hp_bucket)]);    // quality  → watermark per state
```

The controller tracks up to 512 distinct states per seed. For each it records whether any discovery there was **progress** (a watermark, frontier, or quality improvement — monotonic signals like `assert_sometimes_greater_than!(level, 0, "dungeon level reached")`) rather than plain coverage, and how many continuation batches it has received.

## A Few Exemplars Per State

For each state the controller retains up to three **exemplars** — recipes that reach it, anchored at the discovery's RNG count. Three, not one, because of the doomed-state trap: "floor 4 at full health" and "floor 4 about to die" hit the same bucket with dramatically different futures. If the only retained exemplar is doomed, every continuation from that state is wasted.

Once a state's exemplar list is full, new exemplars evict the oldest. Re-discoveries of a known state only arrive through quality and watermark *improvements*, so recency correlates with better starting points — the dying exemplar rotates out when a healthier path to the same floor is found. This is the Metroid lesson (prefer better-resourced paths to the same place) implemented as a three-slot ring buffer instead of an optimization framework.

## Continuations From the Under-Explored Frontier

When the frontier drains and run budget remains, the controller schedules a fresh batch of continuations from the most promising known state:

```text
pick the state minimizing:  visits / (depth + 1)
    ties → progress states first, then deeper recipes
rotate through its exemplars
enqueue branching_factor children with fresh derived seeds
```

The depth weighting is the entire "scoring system": a state whose exemplars sit N replay segments deep gets roughly N+1 times the continuation budget of a shallow one, so effort concentrates on the frontier instead of spreading uniformly — while the visit counter still guarantees every state is revisited occasionally. No energy, no reinforcement learning, no corpus manager.

## The Dungeon Walkthrough

The `dungeon` workload (in `moonpool-sim-examples`) is the acceptance benchmark for this design: eight floors, each hiding a key behind a 3% probability gate, monsters that scale with depth, and a treasure on floor 8 whose brute-force probability is about `0.03^7 ≈ 2×10⁻¹¹`. Reaching it *requires* accumulating progress.

```text
root run:  wanders floor 1, discovers "on key tile floor 1" @ call 812
   └─ continuations replay to the key tile and re-roll the 3% gate
        └─ "key found L1" → "stairs with key" → "descended to_floor 2"
             └─ each new floor adds states, exemplars, deeper anchors
                  └─ ... floor 8, treasure: bug recipe with ~50 segments
```

With `workers: 4` and `max_runs_per_seed: 8000`, the explorer finds the treasure in a few seconds of wall time with at most five live processes. The captured recipe replays the entire path deterministically. The workload itself contributes structure the explorer relies on but never sees: correlated action sequences (70% chance of repeating the previous move — rollouts, not isolated decisions), a stable two-draws-per-step RNG discipline that keeps replay coordinates meaningful, and semantic assertions that name the states worth remembering.
