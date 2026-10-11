# Buggify: Fault Injection

<!-- toc -->

## The Idea

Most distributed system bugs do not live on the happy path. They hide in error handlers, timeout branches, retry loops and recovery code. Those paths run only when something goes wrong, so they are the **least tested** code in the system and the **most critical** when a failure comes.

FoundationDB solved this with BUGGIFY: scatter conditional fault points through the codebase, active only in simulation. Each point perturbs the code in a small way. Return an error instead of success. Add a delay. Shrink a buffer. Run enough seeds and the perturbations push the code through every error path, every retry, every recovery sequence.

Moonpool implements this as the `buggify!` family of macros in the zero-dependency `moonpool-buggify` crate. `moonpool-sim` re-exports them, so `moonpool_sim::buggify!` and `moonpool_buggify::buggify!` share one state.

## Two-Phase Activation

A naive design fires every point on every call. That is chaos, but not useful chaos. If every operation fails, the system never makes progress, and you never test the interactions between partial failures.

Moonpool uses FoundationDB's two-phase model:

**Phase 1: Activation.** The first time a run reaches a buggify location, the location is **activated** or **deactivated** by one draw. The decision holds for the whole run. A deactivated location never fires, however often it is reached. The runner activates each location with a fixed probability of 50% (`buggify_init(0.5)`).

**Phase 2: Firing.** Each time an active location is reached, it fires with a per-site probability. `buggify!` fires at 25%. `buggify_with_prob!` takes its own rate.

```rust
use moonpool_buggify::{buggify, buggify_with_prob};

// Each call site is one location, keyed by file:line.
// First encounter: a 50% activation coin. Then 25% per call while active.
if buggify!() {
    return Err(Error::timeout("simulated timeout"));
}

// Fire at 50% instead of the default 25%.
if buggify_with_prob!(0.5) {
    buffer_size = 1; // force single-byte reads
}
```

Neither rate is a builder setting. The two phases mean each seed tests a **different combination** of active points. Seed 42 activates a request timeout and deactivates a storage error. Seed 43 does the reverse. Run enough seeds and you cover the combinations.

## Four Injection Patterns

FoundationDB's codebase uses BUGGIFY in a few recurring shapes. All of them carry over.

**Error injection on a success path.** After an operation succeeds, sometimes return an error anyway:

```rust
let result = connection.send(message).await;
if result.is_ok() && buggify!() {
    // Force the caller through its error handling.
    return Err(Error::io("buggified send failure"));
}
```

**Artificial delay.** Widen a race window:

```rust
if buggify!() {
    time.sleep(Duration::from_millis(100)).await?;
}
```

**Parameter randomization.** `buggify_pick!` and `buggify_range!` draw a value at an active point, with no `rand` dependency. `buggify_pick!(prob, n)` returns `Some(i)` with `i` in `0..n`. `buggify_range!(prob, lo..hi)` returns `Some(v)` in the range. Both return `None` when the point does not fire, and always outside a simulation:

```rust
use moonpool_buggify::buggify_range;

// Most runs use the configured batch size. Some runs use a tiny one.
let batch_size = buggify_range!(0.25, 1..3).unwrap_or(DEFAULT_BATCH_SIZE);
```

**Alternative code path.** Take a rare path more often:

```rust
let should_compact = needs_compaction() || buggify_with_prob!(0.1);
```

Each of these is a **decision of the code**. The code needs nothing from the environment to take the path. A crash, a failed sync or a cut link is a different thing. The code cannot decide those, so it names the moment with `hint!` and the simulator decides. [hint!: Naming Fault Sites](29-hints.md) draws that line.

## Disruptive Points and the Recovery Tail

A point that makes an operation **fail** (a cut session, a refused request) is different from a point that only takes a rare path. If it keeps firing after the chaos window closes, the liveness checks a workload makes in its quiet tail fail on injection that should have stopped. Mark those points with `buggify_fault_with_prob!`:

```rust
use moonpool_buggify::buggify_fault_with_prob;

if buggify_fault_with_prob!(0.02) {
    return Err(Error::overloaded());
}
```

During the chaos window it behaves like `buggify_with_prob!`. When the runner closes the window, it calls `moonpool_buggify::buggify_enter_recovery`, and from then until the next run every fault point evaluates to `false` without a draw. Ordinary `buggify!` and `buggify_with_prob!` points keep firing through the tail: a rare-but-harmless path is still worth taking while the system recovers. FoundationDB draws the same line by gating its disruptive points on `speedUpSimulation`.

## Named Points a Harness Can Force

Some decisions are made once per seed: hold one journal for the whole window, withhold a whole class of requests. The plain macros key a location by its `file:line`, and each activation is an independent coin. A harness that builds a scenario from several such ingredients waits for the product of their coins. `buggify_named!` keys the location by a stable label instead, so the harness decides it together with the other ingredients:

```rust
use moonpool_buggify::buggify_named;

// Shipped code. With probability 1.0 the location's activation is the
// per-seed decision. Disruptive, so silent in the recovery tail.
if buggify_named!("withhold gc requests", 1.0) {
    return;
}

// Harness, once it drew the scenario for this seed.
moonpool_sim::set_activation("withhold gc requests", scenario);
```

`set_activation(label, active)` replaces the location's own coin. It draws nothing. Call it before the location's first encounter, or later to override. `buggify_init` clears it with every other decision at the start of a run. A point the harness never sets keeps its own coin. Every label must be unique: two points with one label share one activation. [Swarm Testing](28-swarm-testing.md#named-sites-as-a-swarm-axis) shows how a harness uses this as one more swarm axis.

## Spiking Config Knobs

The patterns above scatter `buggify!` through your own code. The simulation has knobs of its own: network latencies, disk IOPS, fault probabilities. FoundationDB randomizes them the same way, with one line at config construction:

```cpp
if (randomize && BUGGIFY) KNOB = deterministicRandom()->random(lo, hi);
```

Moonpool has `buggify_knob!(default, lo..hi)`. It returns `default` on most seeds. When its call site activates and fires it returns a value drawn inside the range:

```rust
// Most seeds keep the sampled IOPS. A few push the disk to a crawl.
self.iops = moonpool_sim::buggify_knob!(self.iops, 100..5_000);
```

You opt in through the same `enable_chaos` list as everything else:

```rust
SimulationBuilder::new()
    .enable_chaos([
        Chaos::Storage(ChaosMode::Swarm),
        Chaos::BuggifyKnobs,
    ])
    .workload(MyWorkload)
    .run()?;
```

`Chaos::BuggifyKnobs` is a modifier, not a surface. It perturbs knobs only on surfaces you already enabled, so it never switches on a family you left off. On the storage surface the spiked knobs are IOPS, bandwidth, the sync-failure rate, stall and throttle rates and durations, and the disk-failure rate. On the network surface they are the clog, partition, random-close and black-hole rates, and the bit-flip rate, which a seed can push from a sampled 0.001% to 0.02% of sends to around 1%, enough for an integrity check above the transport to meet corruption within a bounded run. Bit flips are spiked only on seeds that enabled them.

This is a different coverage axis from swarm testing. **Swarm decides which fault families are on. Knobs decide how hard an enabled knob gets pushed.** A seed can run storage-only with a disk throttled to 100 IOPS instead of 25,000. The two compose. `buggify_knob!` lives in `moonpool-sim`, not in `moonpool-buggify`: it is simulation configuration, not application fault injection.

## Rate Calibration

Not every point should fire at one rate. FoundationDB uses three tiers:

| Tier | Rate | Use |
|------|------|-----|
| High | 5-10% | Common edge cases: buffer boundaries, retry paths |
| Medium | 1% | Standard scenarios: timeouts, connection resets |
| Low | 0.1-0.01% | Rare critical failures: corruption, crash during sync |

The default 25% works for most points. Avoid `buggify_with_prob!(0.9)` on a fault path: a system that fails 90% of attempts never reaches the interactions between partial success and partial failure.

## Tilting Behaviour in Simulation

`moonpool_buggify::is_simulated()` reports whether this thread runs a simulation (buggify is enabled and a random source is installed). It is FoundationDB's `g_network->isSimulated()`. Use it to tilt a rate, a cadence or an extra check in simulation. Never let it change an outcome a caller can observe.

## CPU Charges

A simulated poll costs a small base cost when the [CPU model](26-cpu-and-gray-failure.md) is on. The base cost does not know that one poll decoded a large batch and another bumped a counter. `cpu_charge` lets the code say so:

```rust
use std::time::Duration;

fn fold_batch(records: &[Vec<u8>]) {
    // ... the real work ...
    // A profile says one record costs about 200 ns to fold.
    moonpool_buggify::cpu_charge(Duration::from_nanos(200) * records.len() as u32);
}
```

Inside a simulation with the CPU model on, the process's clock moves forward by the cost times its slowness. What the process does next (a send, a write, a timer) happens after the work. Everywhere else the call costs one thread-local read. Put a charge only on a hot path a profile shows: an encode, a checksum, a fold.

## Production Safety

Buggify is gated on simulation state. Outside a run, every macro evaluates to `false`. The cost is a thread-local read. You leave the call sites in production code, as FoundationDB leaves BUGGIFY in `fdbserver`.

```toml
[dependencies]
moonpool-buggify = "0.8"
```

The crate is pure `std` with zero dependencies, so sans-I/O and production crates depend on it directly, and it compiles for `wasm32-unknown-unknown`. It owns the disabled-by-default state, the macros, `hint!` and `cpu_charge`. When a run starts, `moonpool-sim` installs its seeded random source with `set_random_source`, calls `buggify_init`, and installs the hint and CPU sinks. When the run ends, `buggify_reset` makes everything inert again.

Two rules keep replay exact. Never mix non-deterministic entropy into a buggified branch: `rand::random()` breaks the seed. And do not use buggify for business logic. A branch that changes application semantics belongs in the application, not behind a fault coin.
