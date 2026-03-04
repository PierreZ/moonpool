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
// First encounter: 50% chance of activation (configurable).
// Subsequent encounters at an active site: 25% chance of firing.
if buggify!() {
    return Err(Error::timeout("simulated timeout"));
}
```

The activation probability is configurable per simulation. The firing probability can be customized per call site:

```rust
// Fire at 50% probability instead of the default 25%
if buggify_with_prob!(0.5) {
    buffer_size = 1; // Force single-byte reads
}
```

This two-phase design means each seed tests a **different combination** of active fault injection points. Seed 42 might activate the timeout injection in your RPC layer but deactivate the one in your storage engine. Seed 43 might do the reverse. Run enough seeds and you cover the combinatorial space of fault interactions.

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

### 5. Process Restarts

Trigger restarts at specific points to test crash recovery:

```rust
if buggify_with_prob!(0.01) {
    // Simulate a crash right after writing but before syncing
    return Err(Error::crash("buggified crash after write"));
}
```

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
