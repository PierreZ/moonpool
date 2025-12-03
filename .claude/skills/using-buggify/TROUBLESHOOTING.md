# Buggify Troubleshooting

This guide helps diagnose and fix common issues when using buggify.

## Issue 1: Buggify Never Triggers

### Symptoms

```
Test completes successfully but assertions show:
sometimes_assert!('connection_failure_path') has 0.0% success rate
```

The buggify call site never fired, so error path untested.

### Diagnosis

**Step 1**: Check if code path is reached at all

```rust
if buggify!() {
    tracing::error!("🔥 Buggify FIRED"); // Add temporary logging
    return Err(NetworkError::ConnectionFailed);
}
```

Run test and check logs:
- **No log**: Code path never reached → problem is elsewhere
- **Log appears**: Buggify is firing → problem is with assertion

**Step 2**: Check buggify activation

Buggify has two-stage activation:
1. Location randomly activated at simulation start (yes/no)
2. If activated, fires probabilistically (~25%) each time reached

If location isn't activated for current seed, it won't fire at all.

### Solutions

#### Solution A: Increase Iterations

Run more seeds until location activates:

```rust
SimulationBuilder::new()
    .use_random_config()
    .set_iteration_control(
        IterationControl::UntilAllSometimesReached(10_000)  // More iterations
    )
    .run()
    .await;
```

#### Solution B: Increase Probability

If activated but not firing often enough:

```rust
// Before: Default 25%
if buggify!() {
    return Err(/*...*/);
}

// After: Higher probability
if buggify_with_prob!(0.75) {  // 75% when activated
    return Err(/*...*/);
}
```

#### Solution C: Check Code Coverage

Code path might be unreachable:

```rust
async fn connect(&mut self) -> Result<Connection> {
    // This code never runs if early return happens above
    if buggify!() {
        return Err(NetworkError::ConnectionFailed);
    }

    // ...
}
```

Add logging before buggify to confirm path is reached.

---

## Issue 2: Too Much Chaos

### Symptoms

```
All tests fail
No seeds pass
Can't make any progress
```

Too many buggify sites firing simultaneously, creating impossible conditions.

### Diagnosis

**Check buggify density**:

```rust
// Count buggify calls in module
grep -n "buggify" src/network/peer.rs | wc -l
```

If >10 in single file, might be too dense.

**Check hot paths**:

```rust
// ❌ Bad: Buggify in tight loop
for msg in messages {
    if buggify!() {  // Fires thousands of times!
        return Err(/*...*/);
    }
    process(msg);
}
```

### Solutions

#### Solution A: Reduce Probability

```rust
// Before: Too aggressive
if buggify!() {  // 25% per call
    return Err(/*...*/);
}

// After: More conservative
if buggify_with_prob!(0.1) {  // 10% per call
    return Err(/*...*/);
}
```

#### Solution B: Remove from Hot Paths

```rust
// Before: In loop
for msg in messages {
    if buggify!() {
        return Err(/*...*/);
    }
    process(msg);
}

// After: Outside loop
if buggify!() {
    return Err(/*...*/);
}
for msg in messages {
    process(msg);
}
```

#### Solution C: Make Conditional

Only inject chaos under specific conditions:

```rust
// Only inject chaos under load
if self.queue.len() > 100 && buggify!() {
    return Err(OverloadError::BackpressureActive);
}
```

---

## Issue 3: Non-Deterministic Failures

### Symptoms

```
Same seed produces different results
Can't reproduce failures
Test flakiness
```

Determinism is broken - buggify depends on deterministic execution.

### Diagnosis

**Check 1**: Using production randomness instead of simulation randomness

```rust
// ❌ Wrong: Non-deterministic
use rand::random;
let value = random::<u64>();

// ✅ Correct: Deterministic
let value = random_provider.random_u64();
```

**Check 2**: Using tokio::spawn instead of task provider

```rust
// ❌ Wrong: Non-deterministic scheduling
tokio::spawn(async move {
    work().await
});

// ✅ Correct: Deterministic scheduling
task_provider.spawn_task(async move {
    work().await
});
```

**Check 3**: Using tokio::time instead of time provider

```rust
// ❌ Wrong: Real time
tokio::time::sleep(Duration::from_secs(1)).await;

// ✅ Correct: Simulated time
time_provider.sleep(Duration::from_secs(1)).await;
```

### Solutions

#### Solution A: Fix Random Usage

Replace all `rand::*` with `SimRandomProvider`:

```rust
// Pass random provider to workload
async fn workload(
    random: SimRandomProvider,  // ← Use this
    // ...
) -> SimulationResult<SimulationMetrics> {
    let value = random.random_u64();
    let bool_val = random.random_bool(0.5);
    let range = random.random_range(1..100);

    // ...
}
```

#### Solution B: Fix Task Spawning

Replace all `tokio::spawn` with `task_provider.spawn_task`:

```rust
// Pass task provider to all components
struct Actor {
    task_provider: Arc<dyn TaskProvider>,
}

impl Actor {
    async fn do_work(&self) {
        let task = self.task_provider.spawn_task(async move {
            background_work().await
        });

        task.await.unwrap();
    }
}
```

#### Solution C: Fix Time Usage

Replace all `tokio::time` with `time_provider`:

```rust
// Pass time provider to all components
struct Service {
    time: Arc<dyn TimeProvider>,
}

impl Service {
    async fn operation_with_timeout(&self) -> Result<()> {
        let timeout = Duration::from_secs(5);

        self.time.timeout(timeout, self.long_operation()).await?
    }
}
```

---

## Issue 4: Buggify Breaks Valid Behavior

### Symptoms

```
Test fails with error: "Invalid state after buggify"
Buggify causes system to enter impossible state
```

Buggify injecting failures that shouldn't be possible or aren't handled correctly.

### Diagnosis

**Check error handling**:

```rust
if buggify!() {
    return Err(NetworkError::ConnectionFailed);
}

let conn = establish_connection().await?;
// ← Is caller prepared for ConnectionFailed error?
```

**Check state consistency**:

```rust
self.state = State::Connecting;

if buggify!() {
    return Err(/*...*/);  // State stays "Connecting" on error!
}

self.state = State::Connected;
```

### Solutions

#### Solution A: Add Error Handling

Ensure all buggify-injected errors are properly handled:

```rust
async fn caller() -> Result<()> {
    match self.connect().await {
        Ok(conn) => {
            self.use_connection(conn).await
        }
        Err(NetworkError::ConnectionFailed) => {
            // ✅ Handle the buggify-injected error
            tracing::warn!("Connection failed, trying fallback");
            self.connect_fallback().await
        }
        Err(e) => Err(e),
    }
}
```

#### Solution B: Fix State Management

Ensure state is consistent even when buggify fires:

```rust
async fn connect(&mut self) -> Result<Connection> {
    self.state = State::Connecting;

    if buggify!() {
        self.state = State::Disconnected;  // ✅ Restore valid state
        return Err(NetworkError::ConnectionFailed);
    }

    let conn = establish_connection().await?;
    self.state = State::Connected;

    Ok(conn)
}
```

#### Solution C: Use Defer Pattern

Ensure cleanup happens even on early return:

```rust
async fn operation(&mut self) -> Result<()> {
    self.mark_in_progress();

    // Ensure cleanup happens
    let _guard = scopeguard::guard((), |_| {
        self.mark_completed();
    });

    if buggify!() {
        return Err(/*...*/);  // Cleanup still happens
    }

    // ... normal operation
}
```

---

## Issue 5: Seed Reproduction Not Working

### Symptoms

```
Failing seed: 12345
Set .set_seed(12345) but failure doesn't reproduce
Different behavior with same seed
```

### Diagnosis

**Check 1**: Seed set correctly?

```rust
let report = SimulationBuilder::new()
    .set_seed(12345)  // ✅ Correct
    .set_iteration_control(IterationControl::FixedCount(1))
    .run()
    .await;
```

**Check 2**: External randomness?

If using non-simulation randomness, seed won't control it.

**Check 3**: Test isolation?

Previous tests might affect state.

### Solutions

#### Solution A: Verify Seed Setting

```rust
// Debug: Print seed at start
let report = SimulationBuilder::new()
    .set_seed(12345)
    .set_iteration_control(IterationControl::FixedCount(1))
    .run()
    .await;

println!("Used seed: {}", report.seed);  // Should be 12345
```

#### Solution B: Full Isolation

Run single test in isolation:

```bash
# Run only specific test
cargo nextest run --no-capture test_name
```

#### Solution C: Add Logging

Enable detailed logging to compare runs:

```rust
let _ = tracing_subscriber::fmt()
    .with_max_level(Level::DEBUG)
    .try_init();

let report = SimulationBuilder::new()
    .set_seed(12345)
    .run()
    .await;
```

Compare logs between runs - should be identical.

---

## Issue 6: Buggify Causes Deadlock

### Symptoms

```
Test hangs indefinitely
Timeout after 240 seconds
No progress in simulation
```

Buggify injecting failures that prevent progress.

### Diagnosis

**Check for infinite loops**:

```rust
// Dangerous pattern
loop {
    if buggify!() {
        continue;  // ← Never exits if buggify keeps firing!
    }

    if let Some(msg) = self.receive().await {
        process(msg);
        break;
    }
}
```

**Check for resource deadlock**:

```rust
// Pool exhausted + all operations fail
if buggify!() {
    return Err(PoolError::Exhausted);  // No connections ever released!
}
```

### Solutions

#### Solution A: Add Max Iterations

```rust
// Before: Infinite loop
loop {
    if buggify!() {
        continue;
    }
    if ready() {
        break;
    }
}

// After: Bounded loop
for attempt in 0..100 {
    if buggify!() {
        continue;
    }
    if ready() {
        break;
    }
}
```

#### Solution B: Conditional Buggify

Don't inject failures that prevent all progress:

```rust
// Only fail if alternative path exists
if self.connections.len() > 1 && buggify!() {
    return Err(NetworkError::ConnectionFailed);
}
```

#### Solution C: Reduce Probability

```rust
// Before: Too aggressive
if buggify!() {  // 25% - might fire many times in a row
    return Err(/*...*/);
}

// After: Lower probability
if buggify_with_prob!(0.05) {  // 5% - less likely to block repeatedly
    return Err(/*...*/);
}
```

---

## Issue 7: Buggify Doesn't Find Known Bug

### Symptoms

```
Know bug exists but simulation doesn't catch it
Coverage shows paths are tested
Bug only appears in production
```

Buggify not creating the right conditions to expose bug.

### Diagnosis

**Analyze bug conditions**:
- What sequence of operations causes bug?
- What timing conditions?
- What resource states?

**Check if buggify targets those conditions**:
- Are relevant paths buggified?
- Are timeouts shrunk appropriately?
- Are resource limits reduced?

### Solutions

#### Solution A: Add Targeted Buggify

If bug requires specific sequence, add buggify to force it:

```rust
// Bug: Race between activate and deactivate
async fn activate(&mut self, id: ActorId) -> Result<()> {
    self.state.insert(id, State::Activating);

    // Add delay to widen race window
    if buggify!() {
        self.time.sleep(Duration::from_millis(50)).await;
    }

    // ... rest of activation
}
```

#### Solution B: Increase Operation Diversity

Add more operation types to workload:

```rust
enum Operation {
    // Existing
    Activate(ActorId),
    Deactivate(ActorId),

    // Add operations that trigger bug path
    ActivateAndImmediatelyDeactivate(ActorId),
    ConcurrentActivations(Vec<ActorId>),
}
```

#### Solution C: Adjust Topology

Bug might only appear in specific topologies:

```rust
// Try different topologies
run_with_topology(1, 1).await;  // 1 client, 1 server
run_with_topology(2, 2).await;  // 2 clients, 2 servers
run_with_topology(10, 10).await;  // 10 clients, 10 servers - might expose bug
```

---

## Quick Diagnostic Checklist

When simulation fails with buggify:

- [ ] Can I reproduce with same seed?
- [ ] Are all providers used (not tokio direct calls)?
- [ ] Is buggify in the right place (error paths, not happy paths)?
- [ ] Is probability appropriate (not too high/low)?
- [ ] Is error handling present for all injected errors?
- [ ] Is state consistent after buggify fires?
- [ ] Are assertions tracking buggify coverage?
- [ ] Is module buggify density reasonable (<10 sites)?
- [ ] Could this be infinite loop or deadlock?
- [ ] Does workload generate enough operations?

---

## Getting Help

If issues persist:

1. **Simplify**: Reduce to minimal reproduction
2. **Isolate**: Test single buggify site at a time
3. **Log**: Enable DEBUG logging to see execution
4. **Compare**: Run with/without buggify to isolate effect
5. **Document**: Capture seed, configuration, error message

Remember: Buggify finding bugs is success, not failure! If simulation catches issues, that's the goal.
