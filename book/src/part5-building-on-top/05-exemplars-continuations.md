# Exemplars and Continuations

<!-- toc -->

Expansion alone is momentum. A productive run spawns a few children. If a child is productive, the wave carries forward. But waves die. Four children from the "key found on floor 5" moment can all get eaten by monsters. With plain expansion, that hard-won state is lost. This chapter covers the machinery that **remembers** progress and keeps re-investing in it.

## Semantic States

Every discovery carries a `state_id`. For a slot assertion it is the hash of the message. For a partial `assert_sometimes_all!` combination it is the site plus the truth combination. For `assert_sometimes_each!` it is the bucket hash: message plus identity keys. This is the Castlevania lesson from [The Exploration Problem](./02-exploration-problem.md), applied through the macros you already write. The workload's assertions project the huge concrete state space into a small set of semantic buckets.

```rust,ignore
assert_sometimes_each!("descended",
    [("to_floor", floor)],       // identity: one state per floor
    [("health", hp_bucket)]);    // quality: a watermark per state
```

The controller tracks up to 512 distinct states per seed (`MAX_TRACKED_STATES`). For each state it records whether any discovery there was **progress** (a watermark, frontier or quality improvement) rather than plain coverage, and how many continuation batches it has received.

## A Few Exemplars Per State

For each state the controller keeps up to three **exemplars** (`MAX_EXEMPLARS_PER_STATE`): recipes that reach it, anchored at the discovery's RNG count. Three, not one, because of the doomed-state trap. "Floor 4 at full health" and "floor 4 about to die" hit the same bucket with very different futures. If the only retained exemplar is doomed, every continuation from that state is wasted.

When a state's list is full, a new exemplar evicts the oldest through a rotating cursor. A known state is rediscovered only through a quality or watermark **improvement**, so recency correlates with a better starting point. The dying exemplar rotates out when a healthier path to the same floor appears. This is the Metroid lesson (prefer better-resourced paths to the same place) as a three-slot ring buffer instead of an optimization framework.

## Continuations From the Under-Explored Frontier

When the frontier drains and run budget remains, the controller schedules a fresh batch of continuations from the most promising known state:

```text
pick the state that minimizes:  visits / (depth + 1)
    ties: progress states first, then deeper recipes, then registration order
rotate through that state's eligible exemplars
enqueue branching_factor children with fresh derived seeds
```

The depth weighting is the whole scoring system. A state whose exemplars sit N replay segments deep gets about N+1 times the continuation budget of a shallow one, so effort concentrates on the frontier instead of spreading uniformly. The visit counter still guarantees that every state gets revisited. An exemplar whose recipe already has `max_recipe_len` segments is not eligible. No energy, no reinforcement learning, no corpus manager.

The loop ends when the per-seed budget is spent or when no state has an eligible exemplar left.

## The Dungeon Walkthrough

The `dungeon` workload in `moonpool-sim-examples` is the acceptance benchmark for this design. Ten levels by default (`DEFAULT_TARGET_LEVEL`). On each level below the last, the key tile grants a key with 2% probability (`KEY_FIND_P`). Monsters scale with depth. A treasure waits on the last level. The brute-force chance to reach it is tiny: nine 2% gates in sequence. Reaching it **requires** accumulated progress.

```text
root run:  wanders level 1, discovers "on key tile" floor 1 @ call 812
   +- continuations replay to the key tile and re-roll the 2% gate
        +- "key found" -> "stairs with key" -> "descended" to_floor 2
             +- each new level adds states, exemplars, deeper anchors
                  +- ... level 10, "treasure found": a bug recipe
```

The binary `sim-dungeon-explore` runs it with `workers: 4` and `max_runs_per_seed: 24_000` over 3 seeds. At most five processes are live at any time. The run passes when the treasure is found or when the `"dungeon level reached"` watermark reaches at least level 5. A stall at level 4 or below is a regression in the explorer's guidance, not seed variance.

The workload contributes structure the explorer relies on but never sees:

- correlated action sequences (70% chance to repeat the previous move, `REPEAT_ACTION_P`), so continuations are rollouts rather than isolated decisions;
- a fixed number of RNG draws per step, so replay coordinates stay meaningful;
- semantic assertions that name the states worth remembering: `"on key tile"` and `"stairs with key"` bucketed by floor with a health quality, `"descended"` bucketed by `to_floor`, and the watermarks `"dungeon level reached"` and `"health remaining"`.
