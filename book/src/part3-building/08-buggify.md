# Buggify: Fault Injection

<!-- toc -->

## The Idea

Most distributed system bugs do not live in the happy path. They hide in error handlers, timeout branches, retry logic, and recovery code. These paths are exercised only when something goes wrong, which means they are the **least tested** code in the system and the **most critical** when failures happen.

FoundationDB solved this with a technique called BUGGIFY: scatter conditional fault injection points throughout the codebase, activated only during simulation. Each point perturbs code behavior in a small way: return an error instead of success, add an artificial delay, randomize a buffer size. Run enough seeds and these perturbations force the code through every error path, every retry loop, every recovery sequence.

Moonpool implements this as the `buggify!()` macro.

## Two-Phase Activation

A naive approach would fire every buggify point on every call. That produces chaos but not useful chaos. If every operation fails, the system never makes progress and you never test the interesting interactions between partial failures.

Moonpool uses FoundationDB's two-phase activation model:

**Phase 1: Activation.** The first time a buggify location is encountered during a simulation run, it is randomly **activated** or **deactivated**. This decision is fixed for the entire run. A location that is deactivated will never fire, no matter how many times it is reached.

**Phase 2: Firing.** Each time an activated location is reached, it fires with a fixed probability (25% by default). This means an active buggify point fires roughly one in four times, creating a mix of successful and failed operations.

```rust
// Each call site is a unique location (identified by file:line).
// First encounter: 50% chance of activation.
// Subsequent encounters at an active site: 25% chance of firing.
if buggify!() {
    return Err(Error::timeout("simulated timeout"));
}
```

Neither rate is a builder setting: the runner activates each location at a fixed 50% per seed, and `buggify!` fires an active location at a fixed 25% per call. What can vary is the firing probability, per call site, with `buggify_with_prob!`:

```rust
// Fire at 50% probability instead of the default 25%
if buggify_with_prob!(0.5) {
    buffer_size = 1; // Force single-byte reads
}
```

This two-phase design means each seed tests a **different combination** of
active fault injection points. Seed 42 might activate a request timeout but
deactivate a storage error. Seed 43 might do the reverse. Run enough seeds and
you cover the combinatorial space of fault interactions.

## Five Injection Patterns

FoundationDB's codebase uses BUGGIFY in five recurring patterns, all of which translate directly to moonpool:

### 1. Error Injection on Success Paths

The most common pattern. After a successful operation, sometimes return an error anyway:

```rust
let result = connection.send(message).await;
if result.is_ok() && buggify!() {
    // Force the caller through its error handling path
    return Err(Error::io("buggified send failure"));
}
```

### 2. Artificial Delays

Inject delays to expose race conditions and timing-dependent bugs:

```rust
if buggify!() {
    // Slow down this operation to widen race windows
    time.sleep(Duration::from_millis(100)).await?;
}
```

### 3. Parameter Randomization

Vary sizes, timeouts, and limits to test edge cases:

```rust
let batch_size = if buggify!() {
    // Test with tiny batches to exercise boundary conditions
    random.random_range(1..3)
} else {
    DEFAULT_BATCH_SIZE
};
```

### 4. Alternative Code Paths

Force the system down paths it rarely takes:

```rust
let should_compact = needs_compaction() || buggify_with_prob!(0.1);
if should_compact {
    // Exercise compaction logic more frequently
    compact_storage().await?;
}
```

### 5. Process Restarts: Hints

A crash is not something the code decides: it is the environment's. So
the code does not crash itself. It names the **moment** where a crash
would be interesting, and the simulator decides:

```rust
use moonpool_buggify::hint;

storage.sync().await?;
hint!("batch durable, not sent").await;
send(messages).await;
```

The batch is durable, and its messages are not sent yet. A reboot right
here leaves the peers not knowing what this process holds. Three vetoes
stand between a hint and a reboot, and none of them commands:

1. The point is a disruptive buggify location of its own. It is activated
   once per run, fires at a low rate per call (`hint::POINT_PROB`, 5%, or
   `hint!("label", 0.2)`), and falls silent in the recovery tail.
2. The seed's chaos. The first `Chaos::Attrition` regime whose victim
   filter admits the process decides. A seed whose swarm drew the
   never-reboot regime never reboots on a hint either.
3. The regime's `max_dead` budget, reboot-kind weights and recovery
   delays, exactly as for its own timed reboots.

The harness may add a fourth: a budget the simulator cannot see. A
replicated store can lose copies at only so many replicas before a crash
stops being survivable, and only the harness keeps that count. It
publishes a `HintVeto` in the run's state, and the sink asks it last,
once the regime has agreed. A yes is a reboot, so the harness can count
it as spent:

```rust
use moonpool_sim::HintVeto;

let ledger = ledger.clone();
HintVeto::new(move |ip, _label| ledger.permit_cut(ip)).publish(ctx.state());
```

`moonpool-journal` names three moments inside its own commit: entries
written but not synced, records written but not synced, and a synced
batch whose metainfo is stale. A reboot there tears the batch in flight,
and the journal's recovery (a torn record, a record rebuilt from its
entry, an ambiguous last batch) runs on every seed that activates them.
Every user of the journal gets these power cuts with no code of its own.

When the simulator reboots, the future never resolves: the kill lands
within one scheduler tick, and every write not yet synced resolves by the
disk's crash physics. Otherwise, and always in production, the future
resolves at once. The simulator finds the process through the task being
polled: each task carries the process that spawned it. A hint from a
workload is ignored.

This is `FoundationDB`'s `if (buggify()) throw please_reboot()`, which its
simulated worker turns into a reboot.

## Disruptive Points and the Recovery Tail

A point that makes an operation *fail* (a cut session, a refused request, a crash) is different from one that only takes a rare path: if it keeps firing after the chaos window closes, the liveness checks the workload makes in its quiet tail fail on injection that should have stopped with everything else. Mark those points with `buggify_fault_with_prob!`:

```rust
if buggify_fault_with_prob!(0.02) {
    return Err(Error::overloaded());
}
```

During the chaos window it behaves exactly like `buggify_with_prob!`. When the runner closes the window (the moment it calls `SimWorld::enter_recovery_mode`), it also calls `moonpool_buggify::buggify_enter_recovery`, and from then until the next iteration every fault point evaluates to `false` without drawing. Ordinary `buggify!` and `buggify_with_prob!` points keep firing through the tail: a rare-but-harmless path is still worth taking while the system recovers. FoundationDB draws the same line by gating its disruptive points on `speedUpSimulation`.

## Spiking Config Knobs

The patterns above scatter `buggify!()` through your own code. But the simulation has its own knobs: network latencies, disk IOPS, partition durations, fault probabilities. FoundationDB randomizes these the same way it randomizes everything else, with a one-liner at config construction:

```cpp
if (randomize && BUGGIFY) KNOB = deterministicRandom()->random(lo, hi);
```

Moonpool gives us `buggify_knob!(default, lo..hi)`. It returns `default` on most seeds, but when its call site activates and fires it returns a random value inside the range:

```rust
// Most seeds keep the sampled IOPS. A few push the disk to a crawl.
self.iops = buggify_knob!(self.iops, 100..5_000);
```

This is a different coverage axis from swarm testing. **Swarm decides which fault families are on. Buggify-knobs decides how hard an enabled knob gets pushed.** A seed might run with only storage faults active, and within that, a disk throttled to 100 IOPS instead of the usual 25,000. The two compose, multiplying the configurations a night of seeds explores.

We opt in through the same `enable_chaos` list we use for everything else:

```rust
SimulationBuilder::new()
    .enable_chaos([
        Chaos::Storage(ChaosMode::Swarm),
        Chaos::BuggifyKnobs,
    ])
    .workload(MyWorkload)
    .run()?;
```

On the network surface the knobs include clog, partition, random-close and black-hole rates, and the in-flight bit-flip rate, which a seed can push from FoundationDB's rare 0.01% to around 1% of sends, enough for an integrity check above the transport to meet corruption within a bounded run.

`Chaos::BuggifyKnobs` is a modifier, not a surface of its own. It only perturbs knobs on surfaces we already enabled, so it never silently switches on a fault family we left off. Like every other buggify decision, each spike is deterministic per seed, so a failing seed replays exactly.

## Probability Calibration

Not all buggify points should fire at the same rate. FoundationDB uses a three-tier calibration:

| Tier | Probability | Use Case |
|------|------------|----------|
| High | 5-10% | Common edge cases: buffer boundaries, retry paths |
| Medium | 1% | Standard scenarios: timeout handling, connection resets |
| Low | 0.1-0.01% | Rare critical failures: data corruption, crash during sync |

The default 25% firing probability works well for most injection points. Use `buggify_with_prob!()` when you need more control. High-probability points are useful for paths you want exercised frequently. Low-probability points model events that are individually rare but must be handled correctly.

## Anti-Patterns

**Do not use buggify for business logic.** Buggify is for simulating infrastructure failures, not for feature flags or A/B testing. If the buggified branch changes application semantics rather than injecting a fault, it belongs in your application logic, not in a buggify block.

**Do not use non-deterministic random.** Buggify uses `sim_random()` internally, which is controlled by the simulation seed. Never mix in `rand::random()` or other non-deterministic entropy. That breaks reproducibility.

**Do not use excessive probabilities without reason.** A `buggify_with_prob!(0.9)` that fails 90% of attempts means the system almost never succeeds. That tests error handling but misses the interesting interactions between partial success and partial failure.

## Production Safety

Buggify is gated behind simulation state. When the simulation is not running, `buggify!()` always returns `false`. There is no runtime cost in production: the check is a thread-local boolean read. You can leave buggify calls in your production code without worrying about them firing outside simulation.

This is the same guarantee FoundationDB provides: BUGGIFY is gated behind `g_network->isSimulated()`, ensuring zero production impact regardless of how aggressively chaos is injected during testing.

## The Standalone `moonpool-buggify` Crate

The `buggify!()`, `buggify_with_prob!()` and `buggify_fault_with_prob!()` macros live in the zero-dependency `moonpool-buggify` crate. Sans-I/O and production code that wants buggify points can depend on it directly, without pulling the simulation runtime into its dependency graph:

```toml
[dependencies]
moonpool-buggify = "0.9"
```

The crate owns only the disabled-by-default state and the macros. When a simulation run starts, `moonpool-sim` installs its deterministic seeded RNG into that shared state, so macros imported through either crate share activation decisions during simulation — and stay inert everywhere else. `moonpool-sim` re-exports the macros, so existing `moonpool_sim::buggify!` call sites are unchanged.

The crate also holds `hint!` (above), `is_simulated()`, and `buggify_pick!` / `buggify_range!`, which draw a choice or a value at an active point without a `rand` dependency. Their production probe companions, `reachable!` and `sometimes!`, live in the zero-dependency `moonpool-assertions`: a probe and a simulation's `assert_reachable!` with the same message share one slot.

`buggify_knob!` remains in `moonpool-sim`: knob randomization is simulation-specific configuration spiking, not application-level fault injection.
