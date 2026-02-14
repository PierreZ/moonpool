# Using Assertions

## When to Use This Skill

Invoke when:
- Adding assertions to simulation workloads, invariant checks, or library code
- Choosing between `assert_always!` and `assert_sometimes!` families
- Tracking coverage of error paths, chaos injection, or probabilistic scenarios
- Validating safety invariants that must never be violated
- Using numeric comparisons to guide exploration toward boundary bugs
- Tracking multi-step workflows or per-value exploration buckets

## Quick Reference

| Macro | Panics? | Forks? | Use for |
|-------|---------|--------|---------|
| `assert_always!(cond, msg)` | yes | no | Invariants that must always hold |
| `assert_always_or_unreachable!(cond, msg)` | yes | no | Invariants on optional code paths |
| `assert_sometimes!(cond, msg)` | no | on first success | Coverage targets (path tested at least once) |
| `assert_reachable!(msg)` | no | on first reach | Code path must be reachable |
| `assert_unreachable!(msg)` | yes | no | Code path must never execute |
| `assert_always_greater_than!(val, thresh, msg)` | yes | no | Numeric lower bound (strict) |
| `assert_always_greater_than_or_equal_to!(val, thresh, msg)` | yes | no | Numeric lower bound (inclusive) |
| `assert_always_less_than!(val, thresh, msg)` | yes | no | Numeric upper bound (strict) |
| `assert_always_less_than_or_equal_to!(val, thresh, msg)` | yes | no | Numeric upper bound (inclusive) |
| `assert_sometimes_greater_than!(val, thresh, msg)` | no | on watermark improvement | Value should sometimes exceed threshold |
| `assert_sometimes_greater_than_or_equal_to!(val, thresh, msg)` | no | on watermark improvement | Value should sometimes reach threshold |
| `assert_sometimes_less_than!(val, thresh, msg)` | no | on watermark improvement | Value should sometimes go below threshold |
| `assert_sometimes_less_than_or_equal_to!(val, thresh, msg)` | no | on watermark improvement | Value should sometimes reach threshold |
| `assert_sometimes_all!(msg, [(name, val), ...])` | no | on frontier advance | All named bools should eventually be true simultaneously |
| `assert_sometimes_each!(msg, [(name, val), ...])` | no | on new bucket | Per-value bucketed exploration |

All macros accept `&str` or `String` for message arguments. Macros are exported from `moonpool_sim`.

## Decision Flowchart

```
What are you checking?
|
+-- Must ALWAYS hold (safety invariant)?
|   |
|   +-- Comparing two numeric values?
|   |   +-- YES --> assert_always_greater_than! / _less_than! / etc.
|   |   |          (explorer actively pushes values toward boundary)
|   |
|   +-- Boolean condition?
|   |   +-- Code path always reached? --> assert_always!(cond, msg)
|   |   +-- Code path may not be reached? --> assert_always_or_unreachable!(cond, msg)
|   |
|   +-- Code must never execute? --> assert_unreachable!(msg)
|
+-- Should happen at LEAST ONCE across seeds (coverage)?
    |
    +-- Comparing two numeric values?
    |   +-- YES --> assert_sometimes_greater_than! / _less_than! / etc.
    |   |          (explorer tracks watermark, forks on improvement)
    |
    +-- Multiple sub-goals that should all become true?
    |   +-- YES --> assert_sometimes_all!(msg, [...])
    |
    +-- Per-value bucketed exploration (depth, attempt count)?
    |   +-- YES --> assert_sometimes_each!(msg, [...])
    |
    +-- Simple boolean condition?
    |   +-- YES --> assert_sometimes!(cond, msg)
    |
    +-- Just need to confirm path is reachable?
        +-- YES --> assert_reachable!(msg)
```

## Boolean Assertions

### `assert_always!(condition, message)`

The workhorse invariant assertion. Panics immediately on failure with seed info. Also fails at validation time if the assertion is never reached.

```rust
// State consistency
assert_always!(
    received + in_flight <= sent,
    format!("Message conservation violated: sent={sent}, received={received}, in_flight={in_flight}")
);

// Resource bounds
assert_always!(
    self.open_connections <= self.max_connections,
    format!("Connection limit exceeded: {} > {}", self.open_connections, self.max_connections)
);
```

**Validation contract**: Fails if condition ever false OR if never reached.

### `assert_always_or_unreachable!(condition, message)`

Same as `assert_always!` but does NOT fail if the code path is never reached. Use for invariants on conditional/optional paths.

```rust
// Platform-specific or optional feature path
if let Some(tls) = &self.tls_config {
    assert_always_or_unreachable!(
        tls.cert_chain.len() > 0,
        "TLS configured but no certificates"
    );
}
```

**Validation contract**: Fails if condition ever false. Passes if never reached.

### `assert_sometimes!(condition, message)`

Coverage tracking. Must succeed at least once across all seeds. On first success, triggers a fork to explore alternate timelines from that state. Does not panic.

```rust
// Error path coverage
match self.send(msg).await {
    Ok(()) => { /* ... */ }
    Err(e) => {
        assert_sometimes!(true, "send failure handled");
        // handle error...
    }
}

// Pair with buggify for chaos coverage
if buggify!() {
    assert_sometimes!(true, "buggified_write_failure");
    return Err(IoError::new(ErrorKind::BrokenPipe, "buggified"));
}

// Conditional state coverage
assert_sometimes!(
    self.connections.len() > 1,
    "peer handling multiple simultaneous connections"
);
```

**Validation contract**: Fails if checked but never true (0% success rate).

### `assert_reachable!(message)`

Equivalent to `assert_sometimes!(true, msg)`. Confirms code execution without checking a condition. Place in critical paths: retry logic, error handlers, recovery procedures.

```rust
async fn handle_reconnect(&mut self) {
    assert_reachable!("reconnect_path_exercised");
    // reconnect logic...
}
```

**Validation contract**: Fails if never reached.

### `assert_unreachable!(message)`

Panics immediately when reached. Place in impossible states, dead code paths, error handlers that should never fire.

```rust
match state {
    State::Active => { /* normal */ }
    State::Draining => { /* ok */ }
    State::Zombie => {
        assert_unreachable!("actor reached zombie state");
    }
}
```

**Validation contract**: Fails if ever reached.

## Numeric Assertions

**Why numeric over boolean?** When you write `assert_always!(x < 100, msg)`, the explorer only sees true/false. When you write `assert_always_less_than!(x, 100, msg)`, the explorer sees both values and actively tries to **maximize** x to find boundary violations. This drives exploration toward the exact conditions where bugs hide.

### Always variants (panic on failure)

```rust
// Queue depth must stay below limit
assert_always_less_than!(queue.len(), MAX_QUEUE_SIZE, "queue overflow");

// Latency must stay below SLA
assert_always_less_than!(response_ms, 5000, "SLA violation");

// Balance must stay non-negative
assert_always_greater_than_or_equal_to!(balance, 0, "negative balance");
```

### Sometimes variants (coverage + watermark tracking)

Forks on watermark improvement. The explorer tracks the best value seen and forks when a new extreme is found, driving exploration deeper.

```rust
// Verify queue can actually get loaded
assert_sometimes_greater_than!(queue.len(), 50, "queue reached high depth");

// Verify fast path exists
assert_sometimes_less_than!(response_ms, 10, "fast response achieved");
```

## Compound Assertions

### `assert_sometimes_all!(msg, [(name, bool), ...])`

Tracks a frontier: the maximum number of named booleans simultaneously true. Forks when the frontier advances. Use for multi-step workflows where all conditions should eventually align.

```rust
assert_sometimes_all!("all_nodes_healthy", [
    ("node_a", node_a.is_healthy()),
    ("node_b", node_b.is_healthy()),
    ("node_c", node_c.is_healthy()),
]);

assert_sometimes_all!("full_replication", [
    ("primary_written", primary_ack),
    ("replica_1_caught_up", r1_offset >= primary_offset),
    ("replica_2_caught_up", r2_offset >= primary_offset),
]);
```

### `assert_sometimes_each!(msg, [(name, val), ...])`

Per-value bucketed exploration. Each unique combination of identity keys gets its own bucket. On first discovery of a new bucket, triggers a fork. Optional quality watermarks enable re-forking on improvement within a bucket.

Use for depth tracking where reaching deeper states requires "sequential luck" (multiple successive rare events). The explorer forks at each depth, letting children explore deeper levels independently. This gives an **nth-root asymptotic speedup** over naive random testing.

```rust
// Track reconnection attempt depth
assert_sometimes_each!(
    "backoff_depth",
    [("attempt", state.reconnect_state.failure_count)]
);

// With quality watermarks
assert_sometimes_each!(
    "descended",
    [("to_floor", floor)],
    [("health", hp)]
);
```

## Best Practices

### Message strings must be stable and unique
Messages are used as assertion identity keys in shared memory. Changing a message string creates a new assertion slot.

```rust
// Good: stable string
assert_always!(x > 0, "positive_balance");

// Bad: dynamic string that changes per invocation
assert_always!(x > 0, format!("balance for user {user_id}"));
```

### Prefer numeric over boolean for quantitative bounds
```rust
// Bad: explorer only sees true/false
assert_always!(queue.len() < 1000, "queue overflow");

// Good: explorer sees actual values and drives maximization
assert_always_less_than!(queue.len(), 1000, "queue overflow");
```

### Pair `assert_sometimes!` with `buggify!` blocks
Chaos-injected faults have low effective probability (~0.5%). Forking here explores the error recovery pipeline.

```rust
if buggify!() {
    assert_sometimes!(true, "injected_connection_reset");
    stream.reset();
}
```

### Keep assertions atomic (one property each)
```rust
// Bad: compound condition obscures which part failed
assert_always!(a > 0 && b < 100 && c != 0, "invariants");

// Good: one property per assertion
assert_always_greater_than!(a, 0, "positive_a");
assert_always_less_than!(b, 100, "bounded_b");
assert_always!(c != 0, "nonzero_c");
```

### Don't over-assert
Aim for fewer than 50 total assertions. Focus on meaningful invariants and important coverage targets, not mechanically asserting every line.

### Use `assert_sometimes_each!` for depth tracking
When reaching deeper states requires sequential luck, bucketed assertions give the explorer nth-root speedup by forking at each depth level.

## Validation

After simulation completes, validate all assertion contracts:

```rust
let report = SimulationBuilder::new()
    .register_workload("test", workload)
    .run()
    .await;

panic_on_assertion_violations(&report);
```

Per-kind validation:
- **Always**: fails if condition ever false, or if never reached
- **AlwaysOrUnreachable**: fails if condition ever false
- **Sometimes**: fails if 0% success rate (checked but never true)
- **Reachable**: fails if never reached
- **Unreachable**: fails if ever reached
- **NumericAlways**: fails if comparison ever false
- **NumericSometimes**: fails if 0% success rate
- **BooleanSometimesAll**: no pass/fail contract (frontier is guidance only)

## Source

Macros defined in `moonpool-sim/src/chaos/assertions.rs`. Backing shared-memory infrastructure in `moonpool-explorer/src/assertion_slots.rs`.
