# Using Chaos Assertions

## When to Use This Skill

Invoke this skill when you are:
- Adding assertions to track test coverage during simulation
- Validating safety invariants that must never be violated
- Understanding the difference between sometimes_assert! and always_assert!
- Debugging why certain code paths aren't being tested
- Analyzing assertion coverage reports
- Reproducing failures using specific seeds

## Related Skills

- **designing-simulation-workloads**: Design workloads that trigger assertions
- **using-buggify**: Use buggify to force assertion paths
- **validating-with-invariants**: Use invariants for cross-workload properties

## Philosophy: Two Types of Correctness

Chaos testing tracks two distinct properties:

1. **Coverage**: Did we test every important code path?
2. **Safety**: Do we ever violate correctness properties?

Traditional testing only checks safety. Chaos testing adds **coverage tracking** to ensure probabilistic paths are actually tested.

## The Two Assertion Types

### sometimes_assert! - Coverage Tracking

**Purpose**: Verify that a code path executed at least once across all simulation runs.

**Behavior**:
- Records success/failure statistics
- Must succeed **≥1 time** across all seeds
- Framework validates coverage at end

**Use for**:
- Error handling paths (timeout occurred, connection failed)
- Probabilistic scenarios (switched servers, queue overflow)
- Edge cases (high message rate, multiple connections)
- Buggify paths (chaos injection triggered)

```rust
sometimes_assert!(
    unique_identifier,
    condition,
    "Description of what this tests"
);
```

### always_assert! - Safety Invariants

**Purpose**: Verify that a property **always** holds, never violated.

**Behavior**:
- Panics immediately on failure
- No statistics tracking
- Zero tolerance for violations

**Use for**:
- State consistency (actor not duplicated)
- Conservation laws (messages_received ≤ messages_sent)
- Uniqueness constraints (single location per actor)
- Resource invariants (balance non-negative)

```rust
always_assert!(
    unique_identifier,
    condition,
    "Error message explaining violation"
);
```

## Key Distinction

| Aspect | sometimes_assert! | always_assert! |
|--------|------------------|----------------|
| **Purpose** | Coverage tracking | Safety validation |
| **Success requirement** | ≥1 time across all runs | Every single time |
| **Failure** | Warning (untested path) | Panic (bug found) |
| **Statistics** | Yes (success rate) | No |
| **Use when** | Path should happen sometimes | Property must always hold |

## Using sometimes_assert!

### Basic Pattern

```rust
if buggify!() {
    sometimes_assert!(
        connection_failure_path,
        true,
        "Connection failure path tested"
    );
    return Err(NetworkError::ConnectionFailed);
}
```

**Why**: Verifies that buggify actually triggered and error path executed.

### Error Path Coverage

```rust
match self.send_message(msg).await {
    Ok(()) => {
        sometimes_assert!(
            message_send_success,
            true,
            "Message send succeeded"
        );
    }
    Err(SendError::QueueFull) => {
        sometimes_assert!(
            queue_overflow_handled,
            true,
            "Queue overflow error handled"
        );
    }
    Err(SendError::ConnectionLost) => {
        sometimes_assert!(
            connection_lost_handled,
            true,
            "Connection lost error handled"
        );
    }
    Err(e) => return Err(e),
}
```

**Why**: Ensures all error branches are actually tested.

### Conditional Coverage

```rust
if self.queue.len() > 100 {
    sometimes_assert!(
        high_queue_depth,
        true,
        "System operating under high queue depth"
    );
}

if connections.len() > 1 {
    sometimes_assert!(
        multiple_connections,
        connections.len() > 1,
        "Server handling multiple connections"
    );
}
```

**Why**: Tracks that system reached interesting states.

### Probabilistic Behavior

```rust
if selected_server != last_server {
    sometimes_assert!(
        server_switching,
        true,
        "Client switched between servers"
    );
}

if response_time < Duration::from_millis(10) {
    sometimes_assert!(
        fast_response,
        true,
        "Fast response path (< 10ms)"
    );
}
```

**Why**: Verifies dynamic behavior actually varies as expected.

## Using always_assert!

### State Consistency

```rust
async fn activate_actor(&mut self, id: ActorId) -> Result<()> {
    always_assert!(
        no_duplicate_activation,
        !self.active_actors.contains_key(&id),
        format!("Actor {:?} already active - duplicate activation", id)
    );

    // ... activate actor

    always_assert!(
        actor_now_active,
        self.active_actors.contains_key(&id),
        format!("Actor {:?} not in active set after activation", id)
    );

    Ok(())
}
```

**Why**: These properties must **always** hold, never violated.

### Conservation Laws

```rust
fn validate_message_counts(&self) {
    let sent = self.messages_sent.load(Ordering::SeqCst);
    let received = self.messages_received.load(Ordering::SeqCst);
    let in_flight = self.messages_in_flight.load(Ordering::SeqCst);

    always_assert!(
        message_conservation,
        received + in_flight <= sent,
        format!(
            "Message conservation violated: sent={}, received={}, in_flight={}",
            sent, received, in_flight
        )
    );
}
```

**Why**: Conservation law must hold at every point.

### Resource Bounds

```rust
async fn allocate_resource(&mut self) -> Result<Resource> {
    let allocated_count = self.resources.len();

    always_assert!(
        within_resource_limit,
        allocated_count < self.max_resources,
        format!(
            "Resource limit violated: allocated={}, max={}",
            allocated_count, self.max_resources
        )
    );

    // ... allocate
}
```

**Why**: Exceeding limits indicates bug.

### Uniqueness Constraints

```rust
async fn register_actor(&mut self, actor_id: ActorId, node_id: NodeId) -> Result<()> {
    let locations = self.lookup_all(&actor_id).await?;

    always_assert!(
        single_location,
        locations.len() <= 1,
        format!(
            "Actor {:?} in multiple locations: {:?}",
            actor_id, locations
        )
    );

    // ... register
}
```

**Why**: Multiple locations indicates directory inconsistency.

## Assertion Placement Strategy

### 1. Wrap Error Paths with sometimes_assert!

```rust
async fn operation() -> Result<()> {
    match self.try_operation().await {
        Ok(result) => {
            sometimes_assert!(success_path, true, "Operation succeeded");
            Ok(result)
        }
        Err(Error::Timeout) => {
            sometimes_assert!(timeout_path, true, "Timeout handled");
            Err(Error::Timeout)
        }
        Err(Error::NetworkFailure) => {
            sometimes_assert!(network_failure_path, true, "Network failure handled");
            Err(Error::NetworkFailure)
        }
    }
}
```

### 2. Guard Invariants with always_assert!

```rust
async fn modify_state(&mut self) {
    let before = self.compute_invariant();

    // Modify state
    self.update_internal_state();

    let after = self.compute_invariant();

    always_assert!(
        invariant_preserved,
        before == after,
        "Invariant changed unexpectedly"
    );
}
```

### 3. Track Interesting Conditions

```rust
async fn process_messages(&mut self) {
    if self.queue.len() > self.queue.capacity() / 2 {
        sometimes_assert!(
            queue_half_full,
            true,
            "Queue reached 50% capacity"
        );
    }

    if self.active_connections.len() >= 5 {
        sometimes_assert!(
            many_connections,
            true,
            "System handling ≥5 concurrent connections"
        );
    }
}
```

### 4. Pair with Buggify

```rust
if buggify!() {
    sometimes_assert!(
        buggify_triggered,
        true,
        "Buggify injection triggered"
    );
    return Err(NetworkError::ConnectionFailed);
}
```

## Coverage Validation

### How Framework Validates

After simulation completes:

```rust
let report = SimulationBuilder::new()
    .register_workload("test", workload)
    .run()
    .await;

// Framework checks all sometimes_assert! statements
panic_on_assertion_violations(&report);
```

If any `sometimes_assert!` has 0% success rate → panic with violation report.

### Reading Coverage Reports

```
Assertion Results:
  connection_failure_path: 45.2% (452/1000 seeds)
  timeout_path: 12.8% (128/1000 seeds)
  server_switching: 67.3% (673/1000 seeds)
  multiple_connections: 89.1% (891/1000 seeds)
```

**Good coverage**: All assertions >0%
**Missing coverage**: Any assertion at 0% → path untested

### Improving Coverage

If assertion has 0% success rate:

1. **Increase iterations**:
   ```rust
   .set_iteration_control(
       IterationControl::UntilAllSometimesReached(10_000)
   )
   ```

2. **Increase buggify probability**:
   ```rust
   if buggify_with_prob!(0.75) {  // Higher probability
       sometimes_assert!(rare_path, true, "Rare path tested");
       // ...
   }
   ```

3. **Check if path is reachable**:
   - Add logging to verify code executes
   - Review workload operations
   - Verify topology allows scenario

## Seed Reproduction Workflow

When a seed fails with assertion violation:

### Step 1: Capture Seed

```
Error: always_assert! failed: message_conservation
Seed: 12345
Messages sent=1000, received=1001 (violation!)
```

### Step 2: Replay Seed

```rust
let _ = tracing_subscriber::fmt()
    .with_max_level(Level::ERROR)
    .try_init();

let report = SimulationBuilder::new()
    .set_seed(12345)
    .set_iteration_control(IterationControl::FixedCount(1))
    .register_workload("test", workload)
    .run()
    .await;

println!("{}", report);
```

### Step 3: Analyze

With ERROR logging, see exact sequence:
```
ERROR: Message 42 received twice
ERROR: Actor activation raced
```

### Step 4: Fix Root Cause

Don't just work around symptom - fix the bug.

### Step 5: Verify Fix

Re-run with chaos:
```rust
let report = SimulationBuilder::new()
    .use_random_config()
    .set_iteration_control(
        IterationControl::UntilAllSometimesReached(10_000)
    )
    .run()
    .await;

// Should now pass all seeds
assert!(report.seeds_failing.is_empty());
panic_on_assertion_violations(&report);
```

## Best Practices

### Unique Identifiers

```rust
// ❌ Bad: Generic name
sometimes_assert!(test1, true, "Test 1");
sometimes_assert!(test2, true, "Test 2");

// ✅ Good: Descriptive name
sometimes_assert!(
    client_switches_servers,
    selected_server != last_server,
    "Client switched between servers"
);
```

### Clear Messages

```rust
// ❌ Bad: Vague
always_assert!(check, count == expected, "Check failed");

// ✅ Good: Specific
always_assert!(
    message_conservation,
    received <= sent,
    format!("Message conservation violated: sent={}, received={}", sent, received)
);
```

### Atomic Conditions

```rust
// ❌ Bad: Multiple conditions
sometimes_assert!(
    complex_check,
    queue.len() > 0 && connections.len() > 1 && state == Active,
    "Multiple conditions"
);

// ✅ Good: Single condition per assertion
sometimes_assert!(queue_not_empty, queue.len() > 0, "Queue has messages");
sometimes_assert!(multiple_connections, connections.len() > 1, "Multiple connections");
sometimes_assert!(active_state, state == Active, "System active");
```

### Balanced Coverage

```rust
// Don't assert on every line
// Focus on important paths and states

// ✅ Strategic assertions
async fn send_message(&mut self, msg: Message) -> Result<()> {
    if self.queue.len() > 100 {
        sometimes_assert!(high_load, true, "High load condition");
    }

    match self.transport.send(msg).await {
        Ok(()) => {
            sometimes_assert!(send_success, true, "Send succeeded");
            Ok(())
        }
        Err(e) => {
            sometimes_assert!(send_failure, true, "Send failure handled");
            Err(e)
        }
    }
}
```

## Integration Checklist

When adding assertions to a module:

- [ ] Import macros: `use moonpool_foundation::{sometimes_assert, always_assert};`
- [ ] Add `sometimes_assert!` to all error paths
- [ ] Add `sometimes_assert!` to probabilistic scenarios
- [ ] Add `sometimes_assert!` after buggify injections
- [ ] Add `always_assert!` for state consistency checks
- [ ] Add `always_assert!` for conservation laws
- [ ] Add `always_assert!` for resource bounds
- [ ] Use unique, descriptive identifiers
- [ ] Write clear, specific messages
- [ ] Use atomic conditions (one property per assertion)
- [ ] Balance coverage (don't over-assert)
- [ ] Validate with `panic_on_assertion_violations(&report)`

## Common Patterns

### Error Handling Coverage

```rust
match operation().await {
    Ok(result) => {
        sometimes_assert!(success, true, "Operation succeeded");
    }
    Err(Error::Specific) => {
        sometimes_assert!(specific_error, true, "Specific error handled");
    }
    Err(_) => {
        sometimes_assert!(other_error, true, "Other error handled");
    }
}
```

### State Transition Validation

```rust
async fn transition_state(&mut self) -> Result<()> {
    let old_state = self.state;

    always_assert!(
        valid_old_state,
        old_state.is_valid_for_transition(),
        format!("Invalid transition from {:?}", old_state)
    );

    self.state = new_state;

    always_assert!(
        valid_new_state,
        self.state.is_valid(),
        format!("Invalid state after transition: {:?}", self.state)
    );

    Ok(())
}
```

### Resource Accounting

```rust
fn allocate(&mut self) -> Result<Resource> {
    let before = self.allocated_count;

    let resource = self.do_allocation()?;

    let after = self.allocated_count;

    always_assert!(
        allocation_tracked,
        after == before + 1,
        format!("Allocation not tracked: before={}, after={}", before, after)
    );

    Ok(resource)
}
```

## Key Takeaways

- **sometimes_assert!** = Coverage tracking (path tested ≥1 time)
- **always_assert!** = Safety validation (property never violated)
- **Unique identifiers** for each assertion (enables tracking)
- **Clear messages** explaining what's tested or violated
- **Atomic conditions** (one property per assertion)
- **Strategic placement** (error paths, invariants, interesting states)
- **Framework validation** ensures sometimes_assert! coverage
- **Seed reproduction** enables deterministic debugging

The goal: Prove that all important paths are tested AND that no correctness properties are ever violated!

## Additional Resources

See separate reference files:
- `ASSERTION-PATTERNS.md`: Common assertion patterns by use case
- `COVERAGE-ANALYSIS.md`: How to interpret coverage reports and fix gaps
- `EXAMPLES.md`: Real-world assertions from ping_pong tests
