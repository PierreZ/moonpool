# Multi-Seed Exploration

<!-- toc -->

Exploration runs per root seed. A campaign with `set_iterations(3)` explores three multiverses, one after the other. The naive design would reset everything between seeds and spend the full `max_runs_per_seed` budget three times. Moonpool does not. Novelty is **cumulative**, so later seeds only pay for what earlier seeds did not find.

## Two Kinds of State

The controller keeps two kinds of state, and resets only one of them when a new seed starts.

| State | Scope | Reset by `begin_seed` |
|-------|-------|------------------------|
| Frontier, exemplars, tracked states, statistics, bug recipes, run counter | One seed | Yes |
| Discovery latches in the shared assertion region | The whole campaign | No |
| Sancov history map (highest bucketed count per edge) | The whole campaign | No |

`Explorer::begin_seed(seed)` clears the per-seed search structures and calls `begin_run()` for the root timeline. It deliberately keeps the compare-and-swap latches and the sancov history. The doc comment on the method states the rule: later seeds only get exploration effort for discoveries no earlier timeline made.

## A Barren Seed Costs One Run

Follow a two-seed campaign on the maze workload:

```text
seed 1, root run:   discovers "exit found" on gate 3, "retry fired", "turned back"
                    -> productive, expands, explores up to 2400 timelines
seed 2, root run:   same semantic states, all latches already taken
                    -> journal empty -> barren -> zero children
```

The second seed finishes after exactly one timeline. The report shows it in `report.exploration.per_seed_timelines`, a vector parallel to `report.seeds_used`. A typical print after a campaign:

```text
per_seed_timelines: [1843, 1, 1]
```

The first seed consumed the budget. The two later seeds added nothing new and were not punished for it, but they did not get another 2400 runs either. `max_runs_per_seed` is a **ceiling**, not an allocation. A seed stops as soon as its frontier drains and no tracked state has an eligible exemplar left.

A later seed still earns a full exploration when its root draws a different profile. The network and storage profiles are drawn from the seed before the counted RNG stream starts, so seed 2 may run with partitions where seed 1 ran clean. If that root run discovers a state the first multiverse never reached, the frontier fills again and the seed gets the budget it needs.

## What Cumulative Novelty Buys

The per-seed structures stay small and ordered. The frontier never mixes jobs from two root seeds, every recipe replays against its own seed, and the one-expansion rule and the continuation scoring reason about one multiverse at a time. A bug recipe always names the seed it belongs to:

```rust,ignore
pub struct BugRecipe {
    pub seed: u64,            // the root seed of the multiverse
    pub recipe: Vec<(u64, u64)>,
}
```

Novelty, by contrast, is global. A state discovered under seed 1 is "already known" under seed 2, in the controller process and in every forked worker. The same latch guards the `UntilCoverageStable` convergence check, so running many seeds with exploration does not inflate assertion coverage with duplicates.

## Interplay With UntilCoverageStable

The default iteration control is `UntilCoverageStable { plateau_seeds: 10, max_iterations: 1000 }`. After each root seed (and its exploration), the runner samples one progress signal:

| Signal | When | Source |
|--------|------|--------|
| `CodeCoverage` | sancov counters are present (`cargo xtask sim run`) | with exploration: `sancov_edges_covered()`, the cumulative history map; without: `sancov_edges_covered_live()`, a union of live counters |
| `AssertionCoverage` | no sancov | the number of `assert_sometimes!` and `assert_reachable!` sites that passed |

The campaign converges when every sometimes-style site has passed and the signal has not moved for `plateau_seeds` consecutive seeds. Because the history map is cumulative, the signal is monotonic. Barren seeds move it by zero, so ten barren seeds in a row end the campaign. The `converged` flag in `ExplorationReport` reports which way the loop stopped, and `report.convergence_timeout` is set when `max_iterations` ran out first.

The dungeon binary uses `set_iterations(3)` instead. Three multiverses are enough to show that the treasure depends on accumulated progress and not on a lucky root seed, and a fixed count keeps the CI wall clock predictable.

## The Complete Picture

Put the pieces of this part together:

1. A root seed runs. Its discoveries, stamped with RNG call counts, fill a journal.
2. The controller expands the run once, with `branching_factor` children anchored at the best discovery.
3. Workers replay each child recipe and continue with fresh randomness. Live processes stay at `1 + workers`.
4. Every semantic state keeps three exemplars. When the frontier drains, continuations restart from the least-visited, deepest state.
5. The seed stops at the budget ceiling or when nothing is left to continue.
6. The next seed starts with an empty frontier and a full memory of what is already known.

The result is a campaign that spends compute where new behaviour is, in a fixed process footprint, with a replayable recipe for every bug. The next chapter applies this to a consensus protocol.
