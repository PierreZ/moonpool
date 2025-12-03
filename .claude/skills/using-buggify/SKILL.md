# Using Buggify

## When to Use This Skill

Invoke this skill when you are:
- Adding fault injection to force edge cases and error paths
- Debugging why simulation isn't finding bugs in specific code paths
- Strategic placement of chaos to increase test coverage
- Tuning buggify probabilities for rare events
- Understanding how deterministic fault injection works

## Related Skills

- **designing-simulation-workloads**: Create workloads that benefit from buggify chaos
- **using-chaos-assertions**: Track that buggify actually triggers expected paths
- **validating-with-invariants**: Ensure system maintains correctness under buggify chaos

## Philosophy: Deterministic Fault Injection

Traditional chaos testing has a problem: **random failures alone won't find rare bug combinations.**

**Buggify solves this** by deterministically injecting faults at strategic locations:
- Each `buggify!()` call site is randomly activated once per simulation seed
- When activated, fires probabilistically (default 25%) on each execution
- **Same seed = same activation + same firing sequence = reproducible bugs**

### The Architectural Trick

FoundationDB's key insight: **Interface swapping**

```
Production:
  INetwork → Net2 (real TCP sockets)

Simulation:
  INetwork → Sim2 (simulated network with buggify)
```

Same production code runs in simulation. Buggify calls inject chaos when using Sim2 providers, are no-ops in production.

### Combinatorial Explosion Strategy

Buggify shrinks timeouts 600x and randomizes configuration:
- Production: 60s timeout
- Simulation with buggify: 0.1s timeout

This creates time pressure, exploring thousands of timing combinations impossible to test manually.

## How Buggify Works

### Deterministic Randomness

```rust
// Each call site gets unique location ID (file:line)
buggify!()  // Location: peer.rs:123

// Simulation decides at start: "Activate peer.rs:123? Yes"
// Then fires ~25% of times when executed
```

**Key property**: Same seed → identical decisions → reproducible bugs

### Macro API

```rust
// Default 25% probability when active
buggify!()

// Custom probability when active (50%)
buggify_with_prob!(0.5)

// Higher probability for rare paths (75%)
buggify_with_prob!(0.75)
```

### Basic Usage Pattern

```rust
if buggify!() {
    // Inject failure
    return Err(NetworkError::ConnectionFailed);
}

// Normal path continues
let connection = establish_connection().await?;
```

## Strategic Placement Guide

Place buggify calls where failures **should** be handled gracefully but rarely occur in normal testing.

### 1. Error Handling Paths

Force errors that are normally rare:

```rust
async fn connect_to_peer(&self, addr: Address) -> Result<Connection> {
    if buggify!() {
        tracing::warn!("Buggify: Simulating connection failure");
        return Err(NetworkError::ConnectionFailed);
    }

    let conn = self.transport.connect(addr).await?;

    Ok(conn)
}
```

**Why**: Tests error handling code that might never execute otherwise.

### 2. Timeout Triggers

Shrink timeouts to create time pressure:

```rust
async fn send_with_timeout(&self, msg: Message) -> Result<Response> {
    let timeout = if buggify!() {
        Duration::from_millis(1)   // Very short
    } else {
        Duration::from_secs(5)     // Normal
    };

    self.time.timeout(timeout, self.send(msg)).await?
}
```

**Why**: Exposes timeout handling bugs and race conditions.

### 3. State Transitions

Add delays during critical state changes:

```rust
async fn activate_actor(&mut self, actor_id: ActorId) -> Result<()> {
    // Check if already activating
    if self.catalog.is_activating(&actor_id) {
        return Err(ActorError::AlreadyActivating);
    }

    self.catalog.mark_activating(&actor_id);

    // Race condition window
    if buggify!() {
        self.time.sleep(Duration::from_millis(50)).await;
    }

    // Create actor instance
    let actor = self.factory.create(actor_id.clone()).await?;
    self.catalog.mark_active(&actor_id, actor);

    Ok(())
}
```

**Why**: Widens race condition windows, making timing bugs reproducible.

### 4. Resource Limits

Force resource pressure:

```rust
fn create_message_queue(&self) -> MessageQueue {
    let capacity = if buggify!() {
        2      // Very small - force overflow
    } else {
        1000   // Normal
    };

    MessageQueue::with_capacity(capacity)
}
```

**Why**: Tests queue overflow, backpressure, resource exhaustion handling.

### 5. Network Operations

Simulate send failures:

```rust
async fn send_message(&mut self, msg: Message) -> Result<()> {
    if buggify!() {
        tracing::warn!("Buggify: Simulating send failure");
        return Err(TransportError::SendFailed);
    }

    self.transport.send(msg).await?;

    Ok(())
}
```

**Why**: Tests retry logic, message loss handling, failover.

### 6. Registration/Lookup Failures

Force directory/catalog failures:

```rust
async fn register_actor(&mut self, actor_id: ActorId, node_id: NodeId) -> Result<()> {
    if buggify!() {
        tracing::warn!("Buggify: Simulating registration failure");
        return Err(DirectoryError::RegistrationFailed);
    }

    self.directory.register(actor_id, node_id).await?;

    Ok(())
}
```

**Why**: Tests fallback logic, retry mechanisms, inconsistency handling.

## Actor System-Specific Patterns

### MessageBus Routing

```rust
async fn route_message(&mut self, target: ActorId, msg: Message) -> Result<()> {
    // Force lookup failure
    if buggify!() {
        tracing::warn!("Buggify: Directory lookup failure");
        return Err(RoutingError::ActorNotFound);
    }

    let node = self.directory.lookup(&target).await?;

    // Force wrong node routing
    if buggify!() {
        let random_node = self.select_random_node();
        tracing::warn!("Buggify: Routing to wrong node");
        self.send_to_node(random_node, target, msg).await?;
    } else {
        self.send_to_node(node, target, msg).await?;
    }

    Ok(())
}
```

### ActorCatalog Activation

```rust
async fn get_or_activate(&mut self, actor_id: ActorId) -> Result<ActorHandle> {
    if let Some(handle) = self.actors.get(&actor_id) {
        return Ok(handle.clone());
    }

    // Delay to widen activation race window
    if buggify!() {
        self.time.sleep(Duration::from_millis(100)).await;
    }

    // Check again (someone else might have activated)
    if let Some(handle) = self.actors.get(&actor_id) {
        return Ok(handle.clone());
    }

    // Activate
    let handle = self.activate_new(actor_id.clone()).await?;

    // Force activation failure
    if buggify!() {
        self.actors.remove(&actor_id);
        return Err(ActorError::ActivationFailed);
    }

    Ok(handle)
}
```

### Connection Management

```rust
async fn connect_with_backoff(&mut self, peer: PeerId) -> Result<Connection> {
    let mut backoff = Duration::from_millis(10);

    loop {
        match self.try_connect(peer).await {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                // Shrink backoff to create rapid retries
                if buggify!() {
                    backoff = Duration::from_millis(1);
                }

                self.time.sleep(backoff).await;

                // Exponential backoff
                backoff = backoff * 2;

                // Force give up early
                if buggify!() {
                    return Err(NetworkError::ConnectionFailed);
                }
            }
        }
    }
}
```

## Probability Tuning

Default 25% is good for common paths. Adjust for specific needs:

### High Probability (50-75%) - Rare Events

Use higher probability when the code path rarely executes:

```rust
// This path only executes when queue is nearly full
if queue.len() > queue.capacity() * 9 / 10 {
    // Force overflow more aggressively
    if buggify_with_prob!(0.75) {
        return Err(QueueError::Full);
    }
}
```

### Low Probability (10-25%) - Common Paths

Use default or lower when path executes frequently:

```rust
// This executes on every message send
async fn send(&mut self, msg: Message) -> Result<()> {
    // Don't slow down too much
    if buggify_with_prob!(0.1) {
        return Err(TransportError::SendFailed);
    }

    // ... normal send
}
```

### Conditional Buggify

Combine with regular conditions:

```rust
// Only inject chaos when under load
if self.queue.len() > 100 && buggify!() {
    return Err(OverloadError::BackpressureActive);
}
```

## Tracking Buggify Coverage

Use assertions to verify buggify actually triggers:

```rust
if buggify!() {
    sometimes_assert!(
        buggify_connection_failure,
        true,
        "Buggify triggered connection failure path"
    );
    return Err(NetworkError::ConnectionFailed);
}
```

**Why**: If `buggify_connection_failure` never succeeds, this buggify site never activated or fired.

## Performance Considerations

### Don't Overuse

Too many buggify calls slow simulation:

```rust
// ❌ Bad: Buggify in tight loop
for i in 0..10000 {
    if buggify!() {  // Called 10k times per iteration!
        delay().await;
    }
    process_item(i);
}

// ✅ Good: Buggify before loop
if buggify!() {
    delay().await;
}
for i in 0..10000 {
    process_item(i);
}
```

### Strategic Over Comprehensive

Focus on:
- Error paths that matter
- Race condition windows
- Resource boundaries

Skip:
- Trivial getters/setters
- Pure computation (no I/O)
- Already well-tested paths

## Integration Checklist

When adding buggify to a module:

- [ ] Import macro: `use moonpool_foundation::{buggify, buggify_with_prob};`
- [ ] Identify error handling paths that need testing
- [ ] Add buggify before state transitions (widen race windows)
- [ ] Inject failures in network/directory/catalog operations
- [ ] Shrink timeouts to create time pressure
- [ ] Reduce resource limits (queue sizes, connection pools)
- [ ] Add assertions to track buggify coverage
- [ ] Document intent with comments explaining injected chaos
- [ ] Test with simulation: verify buggify finds bugs
- [ ] Tune probabilities if paths don't trigger

## Common Patterns

### Connection Failure + Retry

```rust
async fn send_with_retry(&mut self, msg: Message) -> Result<()> {
    for attempt in 0..3 {
        if buggify!() {
            return Err(TransportError::SendFailed);
        }

        match self.transport.send(msg.clone()).await {
            Ok(_) => return Ok(()),
            Err(_) if attempt < 2 => {
                let delay = if buggify!() {
                    Duration::from_millis(1)
                } else {
                    Duration::from_millis(100 * (attempt + 1))
                };
                self.time.sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }

    Err(TransportError::RetriesExhausted)
}
```

### Resource Exhaustion + Backpressure

```rust
async fn enqueue_message(&mut self, msg: Message) -> Result<()> {
    let capacity = if buggify!() {
        5  // Force small queue
    } else {
        1000
    };

    if self.queue.len() >= capacity {
        if buggify!() {
            // Reject immediately
            return Err(QueueError::Full);
        }

        // Wait for space
        self.wait_for_space().await?;
    }

    self.queue.push(msg);
    Ok(())
}
```

### Timing-Dependent State

```rust
async fn two_phase_operation(&mut self) -> Result<()> {
    // Phase 1: Prepare
    self.state = State::Preparing;

    if buggify!() {
        self.time.sleep(Duration::from_millis(50)).await;
    }

    self.prepare().await?;

    // Phase 2: Commit
    self.state = State::Committing;

    if buggify!() {
        self.time.sleep(Duration::from_millis(50)).await;
    }

    self.commit().await?;

    self.state = State::Committed;
    Ok(())
}
```

## Debugging Buggify Issues

### Issue: Buggify Not Triggering

**Symptom**: Assertions show certain buggify paths never execute

**Solutions**:
1. Increase probability: `buggify_with_prob!(0.75)`
2. Check if code path is reached at all (add logging)
3. Run more iterations: `UntilAllSometimesReached(10_000)`

### Issue: Too Much Chaos

**Symptom**: All tests fail, can't make progress

**Solutions**:
1. Reduce probability temporarily
2. Remove buggify from hot paths
3. Check for missing error handling

### Issue: Non-Deterministic Failures

**Symptom**: Same seed produces different results

**Solutions**:
1. Verify using `SimRandomProvider`, not `rand::random()`
2. Check for `tokio::spawn()` instead of `task_provider.spawn_task()`
3. Ensure using `time.sleep()` not `tokio::time::sleep()`

## Best Practices

1. **Document intent**: Add comment explaining what chaos is being injected
2. **Use assertions**: Track that buggify actually triggers
3. **Start conservative**: Begin with low probability, increase if needed
4. **Target error paths**: Focus on rarely-tested code
5. **Widen race windows**: Add delays during state transitions
6. **Shrink resources**: Force pressure on queues, connections, buffers
7. **Test incrementally**: Add buggify gradually, verify each addition

## Key Takeaways

- **Buggify biases toward edge cases**: Random chaos alone won't find rare combinations
- **Deterministic**: Same seed = same failures = reproducible bugs
- **Strategic placement**: Error paths, state transitions, resource limits
- **Time pressure**: Shrink timeouts 600x to explore timing combinations
- **Track coverage**: Use assertions to verify buggify triggers

The goal: Force the system through error paths that production will eventually encounter!

## Additional Resources

See separate reference files:
- `PLACEMENT-GUIDE.md`: Detailed decision tree for buggify placement
- `EXAMPLES.md`: Annotated real-world examples from moonpool-foundation
- `TROUBLESHOOTING.md`: Common issues and solutions
