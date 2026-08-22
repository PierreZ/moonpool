# moonpool-explorer

Fork-based multiverse exploration for deterministic simulation testing.

## Why: Explore Beyond Single Seeds

A single simulation seed finds bugs along one execution path. But bugs hide in rare combinations of events. Multiverse exploration forks child processes at discovery points — when an assertion fires for the first time — and explores alternate timelines with different RNG seeds.

This is inspired by [Antithesis](https://antithesis.com/)'s approach: use coverage-guided forking to systematically explore the state space.

## How: Fork at Discovery

When an `assert_sometimes!` succeeds for the first time, the explorer:

1. **Forks** child processes with different RNG seeds
2. Each child **continues** the simulation from the discovery point
3. Children that discover **new coverage** (via bitmap tracking) get more exploration budget
4. **Energy budgets** prevent runaway forking

```text
Parent (seed 42)
├── assert_sometimes!("rare event") fires at RNG count 1000
│   ├── Child A (reseed → 1001) → explores alternate path
│   ├── Child B (reseed → 1002) → finds new bug!
│   └── Child C (reseed → 1003) → no new coverage, stops
└── continues with original seed
```

## Architecture

```text
moonpool-explorer (this crate)  ── leaf, only depends on libc
     ^
moonpool-sim                    ── wires RNG hooks at init
```

This crate has zero knowledge of moonpool internals. Communication with the simulation RNG uses two function pointers set via `set_rng_hooks()`:
- `fn() -> u64` — get current RNG call count
- `fn(u64)` — reseed the RNG

## Key Concepts

- **Coverage bitmap** — 8192-bit vector tracking which code paths have been exercised
- **Explored map** — Tracks frontier of new coverage across all timelines
- **Assertion slots** — 128 shared-memory slots for Antithesis-style assertion tracking (boolean, numeric, compound)
- **Energy budgets** — Three-level system (global, per-mark, realloc pool) prevents runaway forking
- **Adaptive forking** — Batch-based: fork N children, check coverage yield, return energy from barren marks
- **Bug recipes** — Deterministic replay paths in `"count@seed -> count@seed"` format

## Configuration

```rust
ExplorationConfig {
    max_depth: 30,              // Maximum fork depth
    timelines_per_split: 4,     // Timelines per split point (fixed-count mode)
    global_energy: 50_000,      // Total fork budget
    adaptive: Some(AdaptiveConfig {
        batch_size: 20,          // Children per adaptive batch
        min_timelines: 100,      // Minimum timelines per mark
        max_timelines: 200,      // Maximum timelines per mark
        per_mark_energy: 1000,   // Energy budget per fork point
        warm_min_timelines: None, // Override min_timelines for warm seeds
    }),
    parallelism: None,          // Optional multi-core sliding window
}
```

## Documentation

- [API Documentation](https://docs.rs/moonpool-explorer)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
