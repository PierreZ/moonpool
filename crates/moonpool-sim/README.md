# moonpool-sim

Deterministic simulation engine for distributed systems, inspired by [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).

## Why: Discovery Testing, Not Prevention Testing

Traditional testing asks: *"Did we break what used to work?"* This is **prevention testing**—regression detection through test coverage.

Simulation testing asks: *"What else is broken that we haven't found yet?"* This is **discovery testing**—actively hunting for unknown bugs.

Bugs hide in rare combinations of events. An API with just six variables creates thousands of unique test cases for happy paths alone. Every new feature multiplies the complexity. Traditional testing cannot cover these combinations—but simulation can explore them autonomously.

## How: Deterministic Simulation

**Same seed = identical execution.** Given the same seed, the system makes identical decisions every time. When a bug surfaces after millions of simulated operations, you can replay that exact sequence to debug it.

**Time compression.** A single-threaded event loop advances simulated time when all tasks block. Years of uptime can be simulated in seconds. A simulated day passes instantly when nothing is scheduled.

**Chaos injection.** The simulator deliberately biases execution toward rare code paths. Network delays, disconnects, partitions, bit flips, storage corruption—failures that might take months to occur in production happen continuously in simulation.

## Runtime Architecture

`SimWorld` is the coordinator, not a bucket for every subsystem's mutable
state. Its global `Scheduler<Event>` owns logical time, deterministic same-time
FIFO ordering, and cancellation. It dispatches targeted events to two
independent engines:

- `NetworkSimulation` owns listeners, connections, topology, network faults,
  pending operations, results, and network wakers. Bind, connect, and accept
  complete only after their configured simulated latency.
- `StorageEngine` owns persistent file contents, independent open handles and
  cursors, per-process disk configuration, degradation episodes, exact pending
  operation results, and storage wakers. Read, write, sync, and set-length
  completions identify the operation that submitted them.

Both engines return ordered scheduling and cancellation effects plus wake
batches to the coordinator. The coordinator applies those effects and invokes
wakers after releasing the world lock. Shutdown and process crashes cancel or
fail pending work instead of treating a missing completion as success.

## Controlled Failure Injection: BUGGIFY

Rather than hoping rare bugs surface, moonpool deliberately triggers them. `buggify!` points fire with 25% probability during testing, creating a combinatorial explosion across configurations.

Strategic placement at error-prone points ensures deep bugs—those needing rare combinations of events—actually get tested.

## The Assertion Suite

Moonpool provides 15 Antithesis-style assertion macros for comprehensive property testing:

**Boolean assertions** — guard correctness properties:
- `assert_always!` / `assert_always_or_unreachable!` — invariants that must never fail
- `assert_sometimes!` — verify that edge cases actually occur
- `assert_reachable!` / `assert_unreachable!` — code path reachability

**Numeric assertions** — track watermarks and thresholds:
- `assert_always_greater_than!`, `assert_always_less_than!` (and `_or_equal_to` variants) — numeric invariants
- `assert_sometimes_greater_than!`, `assert_sometimes_less_than!` (and `_or_equal_to` variants) — watermark tracking with replay anchors on improvement

**Compound assertions** — multi-condition discovery:
- `assert_sometimes_all!` — frontier tracking across multiple named conditions
- `assert_sometimes_each!` — per-value bucketed assertions with quality watermarks

If your error handling code exists but `assert_sometimes!` never fires, you haven't actually tested it. The goal is 100% sometimes coverage—proof that every error path was exercised.

## Multi-Seed Testing

Different seeds explore different execution orderings. The default `UntilCoverageStable` stop condition runs simulations with different seeds until every `assert_sometimes!` / `assert_reachable!` has triggered at least once **and** code coverage has plateaued (capped at `max_iterations`). Tune it with `.until_coverage_stable(plateau_seeds, max_iterations)`.

This transforms testing from "check known behaviors" to "explore the unknown until confident."

## Multiverse Exploration

Beyond multi-seed testing, moonpool-sim integrates with `moonpool-explorer` for frontier-based exploration. When an assertion discovers new behavior, the explorer records an RNG-coordinate recipe. Bounded workers replay that prefix from the beginning and continue with different deterministic randomness.

The controller expands productive runs once, retains a few semantic-state exemplars, and caps total runs, queued jobs, and recipe depth. Code-edge coverage is reported and drives plateau detection; semantic assertions provide the replay anchors that guide the frontier.

This is assertion-guided randomized testing, not exhaustive model checking.
Exploration requires `.workload_factory()` or `.workloads()` plus built-in
chaos surfaces; opaque workload instances and custom fault-injector instances
are rejected because Moonpool cannot reconstruct them for every continuation.

## Documentation

- [API Documentation](https://docs.rs/moonpool-sim)
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
