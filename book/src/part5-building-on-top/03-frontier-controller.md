# The Frontier Controller

<!-- toc -->

The heart of the explorer is a small loop that runs in one process and owns every exploration decision. Nothing else in the system decides anything: assertion macros *describe* interesting behavior, workers *execute* timelines, and the controller — the `Explorer` in `moonpool-explorer` — turns discoveries into follow-up work.

```text
                explorer / controller
                        |
                     frontier          (FIFO queue of jobs)
                        |
            +-----------+-----------+
            |           |           |
          worker      worker      worker
            |           |           |
            +-----------+-----------+
                        |
               discovery journals
                        |
                novelty decision
                        |
          productive? → expand once
          barren?     → the branch dies
```

## Jobs Are Recipes

The unit of exploration work is deliberately minimal:

```rust,ignore
pub struct ExploreJob {
    pub recipe: Recipe,   // Vec<(rng_call_count, seed)>
}
```

A job says: *replay this prefix, then keep going with the fresh randomness of the final segment's seed*. Executing a job means running one complete simulation from the beginning with the recipe's RNG breakpoints installed. Because the simulation is deterministic and every uncounted RNG stream (executor scheduling, `select!` offsets, swarm masks) is a pure function of the root seed and the replayed prefix, replay is *exact* — and cheap enough that no live-checkpoint machinery is needed.

## The Discovery Journal

During a run, every globally-new discovery is recorded into a per-run journal: what kind of discovery it was, which semantic state it belongs to, and the RNG call count at that moment. The call count is the crucial part — it is the coordinate where a child recipe can anchor:

```text
root run (seed 42):
  journal: [floor 2 entered @ call 812, key found @ call 1490]

expansion, anchored at the LATEST discovery (call 1490):
  child recipes: 1490@S1, 1490@S2, 1490@S3, 1490@S4
```

Each child replays through *every* state the parent reached — the earlier discoveries are locked into the prefix — and diverges just past the deepest one.

## One Expansion Per Run

A run that made at least one semantic assertion discovery is **productive** and is expanded **exactly once**: `branching_factor` children are enqueued, anchored at its latest discovery. This holds no matter how many discoveries the run made. If one timeline discovers five new assertion states, it is still *one productive execution*, not five branching events. Sanitizer code coverage is reported separately and does not create frontier jobs.

A run with an empty journal discovered nothing the multiverse had not already seen. It is not punished, scored, or refilled — its branch simply produces no children and dies. This one rule replaces the entire energy system of the previous explorer (global budgets, per-mark allowances, reallocation pools, productive/barren classification): the only global limits are a per-seed run budget and a frontier size cap.

```rust,ignore
ExplorationConfig {
    workers: 0,             // deterministic in-process mode (the default)
    max_runs_per_seed: 8000, // total timelines per root seed
    branching_factor: 4,    // children per productive run
    max_frontier: 1024,     // queued-job cap
    max_recipe_len: 64,     // depth cap in replay segments
}
```

Set `workers` above zero only in a standalone simulation binary when bounded
parallel `fork()` workers are worth the less reproducible search order.

## Who Decides Novelty

Discovery detection lives in the shared assertion region (`moonpool-assertions`): each distinct discovery — a slot's first pass, a new bucket, a watermark improvement — is guarded by an atomic compare-and-swap latch. Whichever timeline wins the CAS journals the discovery; every other timeline, in any process, sees "already known". There is no check-then-merge race and no coverage bitmap to reconcile: the latch *is* the novelty decision, made exactly once, and the controller is the single owner of everything built on top of it (frontier, exemplars, statistics, bug recipes).

## Bug Recipes

A failing exploration run — a workload error, an `assert_always!` violation, or a worker crash — is recorded with its job's recipe verbatim. Replaying it reproduces the failing timeline bit-for-bit:

```rust,ignore
SimulationBuilder::new()
    .replay_timeline(bug.seed, bug.recipe.clone())
    .workload(MyWorkload)
    .run();
```

The next chapter looks at how jobs are physically executed while keeping the process count fixed.
