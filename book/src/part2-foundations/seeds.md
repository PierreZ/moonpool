# Seed-Driven Reproducibility

<!-- toc -->

- One `u64` seed controls everything: RNG, fault injection, timing, connection behavior
- Same seed = same execution = same bugs
- The RNG flows through the system: `set_sim_seed(seed)`, `sim_random()`, `sim_random_range()`
- Call count tracking for debugging: `get_rng_call_count()` tells you exactly where divergence happens
- Multi-seed testing: `UntilAllSometimesReached(N)` runs different seeds until all sometimes assertions fire
