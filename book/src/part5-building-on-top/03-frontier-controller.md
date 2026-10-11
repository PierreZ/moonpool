# The Frontier Controller

<!-- toc -->

The heart of the explorer is a small loop in one process. It owns every exploration decision. Nothing else decides anything: assertion macros **describe** interesting behavior, workers **execute** timelines, and the controller, the `Explorer` type in `moonpool-explorer`, turns discoveries into follow-up work.

```text
                controller (Explorer)
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
           productive? -> expand once
           barren?     -> the branch dies
```

## Jobs Are Recipes

The unit of work is minimal:

```rust,ignore
pub struct ExploreJob {
    pub recipe: Recipe,   // Vec<(rng_call_count, seed)>
}
```

A job says: replay this prefix, then keep going with the fresh randomness of the final segment's seed. To execute a job, the runner runs one complete simulation from the start with the recipe installed as RNG breakpoints (`set_rng_breakpoints`). When the call count passes a breakpoint's target, the stream reseeds and the count restarts at 1.

Every decision in a run is a counted draw on the one simulation stream: executor scheduling, `select!` offsets, swarm masks, fault coins, the workload's own draws. So replay is **exact**, and cheap enough that no live checkpoint is needed. The network and storage profiles are drawn **before** the reset that starts the count, so a breakpoint can never land on them. Attrition, outage, CPU and gray-failure regimes are drawn after it, so they replay too.

## The Discovery Journal

During a run, every globally new discovery goes into a per-run journal as a `DiscoveryEvent`: the `kind`, the semantic `state_id`, and the `call_count` at that moment. The call count is the coordinate where a child recipe can anchor.

```text
root run (seed 42):
  journal: [floor 2 entered @ call 812, key found @ call 1490]

expansion, anchored at the highest-priority discovery:
  progress > structured state novelty > one-shot coverage
  the latest call count wins inside a class
```

The six `DiscoveryKind` values fall into three classes:

| Class | Kinds | Example |
|-------|-------|---------|
| Progress | `WatermarkImprovement`, `FrontierAdvance`, `BucketQuality` | a numeric watermark moves, more `assert_sometimes_all!` facts true at once, a bucket's quality improves |
| Structured novelty | `BucketFirst`, `BooleanCombination` | a new `assert_sometimes_each!` bucket, a new partial truth combination |
| Coverage | `SometimesPass` | the first pass of `assert_sometimes!` or `assert_reachable!` |

This order stops a late, easy coverage hit from pulling the children away from an earlier step toward a hard goal. Every discovery still registers its own exemplar for later continuation scheduling.

The journal holds at most 256 entries (`MAX_JOURNAL_ENTRIES`). Repeated events for one state coalesce to the best anchor. When the journal is full, a stronger discovery replaces the weakest retained one. Early coverage noise can never hide late progress.

## One Expansion Per Run

A run with at least one discovery is **productive**. The controller expands it **exactly once**: it enqueues `branching_factor` children, anchored at the highest-priority discovery. A run that made five discoveries is still one productive execution, not five branching events. The child seeds are derived by hashing the root seed, the base seed, the anchor, the state id, the continuation round and the child index, so the frontier is deterministic too.

A run with an empty journal found nothing the multiverse had not seen. It is not punished or scored. Its branch produces no children and dies. There is no energy accounting. The only global limits are the per-seed run budget, the frontier size and the recipe length.

```rust,ignore
ExplorationConfig {
    workers: 0,              // default: in-process, deterministic
    max_runs_per_seed: 512,  // default
    branching_factor: 4,     // default
    max_frontier: 1024,      // default
    max_recipe_len: 64,      // default
}
```

These are the `ExplorationConfig::default()` values. `enable_exploration` panics on a zero bound; `workers: 0` is valid. The dungeon binary uses `max_runs_per_seed: 24_000` with 4 workers, the maze binary 2400 with none.

## Who Decides Novelty

Novelty detection lives in the shared assertion region of `moonpool-assertions`. First passes, new buckets, watermark improvements, frontier advances and partial truth combinations are guarded by atomic compare-and-swap latches. The timeline that wins the latch journals the discovery. Every other timeline, in any process, sees "already known". There is no check-then-merge race.

The controller is the single owner of everything built on top: frontier, exemplars, statistics and bug recipes. The runner installs one hook so the journal can stamp each discovery: `set_rng_count_hook(rng_call_count)`.

## Bug Recipes

A failing exploration run (a workload error, an `assert_always!` violation, or a worker crash) is recorded with its job's recipe. The controller keeps at most 4 per seed (`MAX_BUG_RECIPES`). The root seed is marked failed in the main report, and `report.exploration.bug_recipes` carries each `BugRecipe { seed, recipe }`. Replay reproduces the failing timeline bit for bit:

```rust,ignore
let bug = report.exploration.as_ref().and_then(|e| e.bug_recipes.first())?;

SimulationBuilder::new()
    .workload_factory(|| Box::new(MyWorkload::default()))
    .replay_timeline(bug.seed, bug.recipe.clone())
    .run()?;
```

`replay_timeline` sets one seed, one iteration, and installs the recipe for that run.

The next chapter looks at how jobs run while the process count stays fixed.
