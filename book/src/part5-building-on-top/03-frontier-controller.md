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

A job says: *replay this prefix, then keep going with the fresh randomness of the final segment's seed*. Executing a job means running one complete simulation from the beginning with the recipe's RNG breakpoints installed. Because the simulation is deterministic and every decision — executor scheduling, `select!` offsets, swarm masks, faults, the workload's own draws — is a counted draw on the one simulation stream, replay is *exact* — and cheap enough that no live-checkpoint machinery is needed.

## The Discovery Journal

During a run, every globally-new discovery is recorded into a per-run journal: what kind of discovery it was, which semantic state it belongs to, and the RNG call count at that moment. The call count is the crucial part — it is the coordinate where a child recipe can anchor:

```text
root run (seed 42):
  journal: [floor 2 entered @ call 812, key found @ call 1490]

expansion, anchored at the highest-priority discovery:
  progress > structured state novelty > one-shot coverage
  latest call count breaks ties inside a class
```

Numeric watermark, compound frontier, and bucket-quality improvements are progress. New `assert_sometimes_each!` buckets and partial `assert_sometimes_all!` combinations are structured state novelty. A first boolean sometimes/reachable pass is one-shot coverage. This ordering prevents a late, easy coverage hit from pulling immediate children away from an earlier step toward a difficult goal. Every discovery still gets its own retained exemplar for later continuation scheduling.

The journal is bounded to 256 entries. Repeated events for one semantic state are coalesced to its best anchor. If the journal fills, stronger discoveries replace the weakest retained coverage events, so early noise cannot permanently hide late progress after the shared novelty latch has fired.

## One Expansion Per Run

A run that made at least one semantic assertion discovery is **productive** and is expanded **exactly once**: `branching_factor` children are enqueued, anchored at its highest-priority discovery. This holds no matter how many discoveries the run made. If one timeline discovers five new assertion states, it is still *one productive execution*, not five branching events. Sanitizer code coverage is reported separately and does not create frontier jobs.

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

Discovery detection lives in the shared assertion region (`moonpool-assertions`): first passes, new buckets, comparison-distance improvements, compound-frontier advances, and partial truth combinations are guarded by atomic shared state. Whichever timeline records the transition journals it; every other timeline, in any process, sees "already known". There is no check-then-merge race, and the controller is the single owner of everything built on top of these signals (frontier, exemplars, statistics, bug recipes).

## Bug Recipes

A failing exploration run — a workload error, an `assert_always!` violation, or a worker crash — is recorded with its job's recipe verbatim. Replaying it reproduces the failing timeline bit-for-bit:

```rust,ignore
SimulationBuilder::new()
    .replay_timeline(bug.seed, bug.recipe.clone())
    .workload(MyWorkload)
    .run();
```

The next chapter looks at how jobs are physically executed while keeping the process count fixed.
