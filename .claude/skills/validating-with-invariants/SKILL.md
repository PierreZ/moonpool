# Validating with Invariants

## When to Use This Skill

Invoke this skill when you are:
- Designing global system properties that span multiple actors
- Creating cross-workload validation (properties that hold across entire simulation)
- Understanding when to use invariants vs assertions
- Implementing the JSON-based invariant system architecture
- Debugging invariant violations

## Related Skills

- **designing-simulation-workloads**: Design workloads that invariants validate
- **using-chaos-assertions**: Use assertions for per-actor validation
- **using-buggify**: Inject chaos to stress-test invariants

## Philosophy: Mathematical Properties vs Assertions

**Invariants** are mathematical properties that describe global system correctness:
- Span multiple actors/components
- Checked after every simulation event
- Detect bugs through property violation

**Different from assertions**:
- Assertions: Per-actor, inline validation
- Invariants: Cross-actor, centralized validation

## When to Use Invariants vs Assertions

### Use Invariants For:

**Cross-actor properties**:
```rust
// Global property: Total messages sent ≥ total messages received
invariant_check!(
    message_conservation,
    total_sent >= total_received
);
```

**System-wide constraints**:
```rust
// Global property: Each actor in at most one location
invariant_check!(
    single_location_per_actor,
    all_actors.iter().all(|a| locations(a).len() <= 1)
);
```

**Distributed state consistency**:
```rust
// Global property: Sum of all account balances matches initial capital
invariant_check!(
    money_conservation,
    accounts.values().sum() == initial_capital
);
```

### Use Assertions For:

**Per-actor validation**:
```rust
// Local property: This actor's queue not overflowed
always_assert!(
    queue_not_overflowed,
    self.queue.len() <= self.queue.capacity(),
    "Queue overflow"
);
```

**Immediate feedback**:
```rust
// Fail fast on local violation
always_assert!(
    state_valid,
    self.state.is_valid(),
    "Invalid state"
);
```

## Invariant Thinking

Transform assumptions into mathematical properties:

### Example: Actor Directory

**Assumption**: "Each actor exists in at most one location"

**Invariant**:
```rust
fn check_single_location(state: &SystemState) -> Result<()> {
    for actor_id in &state.all_actors {
        let locations = state.directory_entries
            .iter()
            .filter(|(aid, _)| aid == actor_id)
            .count();

        if locations > 1 {
            return Err(format!(
                "Actor {:?} in {} locations",
                actor_id, locations
            ));
        }
    }
    Ok(())
}
```

### Example: Message Conservation

**Assumption**: "Messages aren't duplicated or created from nothing"

**Invariant**:
```rust
fn check_message_conservation(state: &SystemState) -> Result<()> {
    let sent: u64 = state.all_nodes
        .iter()
        .map(|n| n.messages_sent)
        .sum();

    let received: u64 = state.all_nodes
        .iter()
        .map(|n| n.messages_received)
        .sum();

    if received > sent {
        return Err(format!(
            "Message conservation violated: sent={}, received={}",
            sent, received
        ));
    }

    Ok(())
}
```

## JSON-Based Invariant System (Moonpool Architecture)

### How It Works

```
Actor 1 ──┐
Actor 2 ──┼─► StateRegistry (JSON) ─► InvariantCheck Functions ─► Panic on violation
Actor 3 ──┘
```

1. **Actors expose state via JSON**
2. **StateRegistry aggregates** all actor states
3. **InvariantCheck functions validate** global properties
4. **Panic** if invariant violated

### Architecture Components

**State Registry**:
```rust
pub struct StateRegistry {
    states: HashMap<String, serde_json::Value>,
}

impl StateRegistry {
    pub fn register(&mut self, actor_id: String, state: serde_json::Value) {
        self.states.insert(actor_id, state);
    }

    pub fn get_all(&self) -> &HashMap<String, serde_json::Value> {
        &self.states
    }
}
```

**Invariant Check Function**:
```rust
type InvariantCheck = fn(&StateRegistry) -> Result<(), String>;

fn check_invariant_name(registry: &StateRegistry) -> Result<(), String> {
    // Extract states
    let states = registry.get_all();

    // Validate property
    for (actor_id, state) in states {
        if violates_property(state) {
            return Err(format!("Violation in {}: ...", actor_id));
        }
    }

    Ok(())
}
```

**Registration**:
```rust
let mut checks: Vec<InvariantCheck> = vec![];
checks.push(check_message_conservation);
checks.push(check_single_location);
checks.push(check_no_resource_leaks);

// Run after every simulation event
for check in &checks {
    check(&state_registry)?;
}
```

## Moonpool's Seven Bug Detectors

From the codebase's JSON-based invariant system:

1. **Message conservation**: `sent ≥ received` globally
2. **Per-peer accounting**: Each peer's sent/received balanced
3. **In-transit tracking**: Messages in flight accounted for
4. **No negative counts**: All counters ≥ 0
5. **Single location**: Actors not duplicated across nodes
6. **Resource bounds**: Within configured limits
7. **State consistency**: No invalid state combinations

## Performance Consideration

**Invariants run after every simulation event** → design efficiently:

```rust
// ❌ Bad: O(n²) check
fn check_slow(registry: &StateRegistry) -> Result<(), String> {
    for actor1 in registry.get_all() {
        for actor2 in registry.get_all() {  // Nested loop!
            // ...
        }
    }
    Ok(())
}

// ✅ Good: O(n) check
fn check_fast(registry: &StateRegistry) -> Result<(), String> {
    let mut seen = HashSet::new();
    for (id, state) in registry.get_all() {
        if seen.contains(id) {
            return Err(format!("Duplicate: {}", id));
        }
        seen.insert(id);
    }
    Ok(())
}
```

**Guidelines**:
- Keep checks O(n) or O(n log n)
- Avoid nested loops over all actors
- Cache computed properties if needed

## Basic Invariant Patterns

### Pattern 1: Conservation Law

```rust
fn check_resource_conservation(registry: &StateRegistry) -> Result<(), String> {
    let total_allocated: u64 = registry.get_all()
        .values()
        .filter_map(|s| s["allocated"].as_u64())
        .sum();

    let total_freed: u64 = registry.get_all()
        .values()
        .filter_map(|s| s["freed"].as_u64())
        .sum();

    let currently_held: u64 = registry.get_all()
        .values()
        .filter_map(|s| s["held"].as_u64())
        .sum();

    if currently_held != total_allocated - total_freed {
        return Err(format!(
            "Resource leak: held={}, allocated={}, freed={}",
            currently_held, total_allocated, total_freed
        ));
    }

    Ok(())
}
```

### Pattern 2: Uniqueness Constraint

```rust
fn check_unique_identifiers(registry: &StateRegistry) -> Result<(), String> {
    let mut seen_ids = HashSet::new();

    for (actor_id, state) in registry.get_all() {
        if let Some(id) = state["unique_id"].as_str() {
            if !seen_ids.insert(id) {
                return Err(format!(
                    "Duplicate ID {} (actor {})",
                    id, actor_id
                ));
            }
        }
    }

    Ok(())
}
```

### Pattern 3: Bounded Property

```rust
fn check_queue_bounds(registry: &StateRegistry) -> Result<(), String> {
    for (actor_id, state) in registry.get_all() {
        let queue_len = state["queue_length"].as_u64().unwrap_or(0);
        let max_capacity = state["queue_capacity"].as_u64().unwrap_or(u64::MAX);

        if queue_len > max_capacity {
            return Err(format!(
                "Queue overflow in {}: {} > {}",
                actor_id, queue_len, max_capacity
            ));
        }
    }

    Ok(())
}
```

## Integration Checklist

When adding invariants:

- [ ] Identify cross-actor properties (not per-actor)
- [ ] Express as mathematical property
- [ ] Implement efficient check function (O(n) preferred)
- [ ] Register check with invariant system
- [ ] Test that violation is detected
- [ ] Verify performance (check runs after every event)

## Best Practices

1. **Start simple**: Begin with 1-2 key invariants
2. **Test violations**: Temporarily break property, verify detection
3. **Efficient checks**: Avoid expensive computations
4. **Clear errors**: Detailed messages explaining violation
5. **Complement assertions**: Invariants are global, assertions are local

## Key Takeaways

- **Invariants**: Cross-actor mathematical properties
- **Assertions**: Per-actor inline validation
- **When to use**: Distributed state, conservation laws, uniqueness
- **Architecture**: JSON state → Registry → Check functions → Panic
- **Performance**: Keep checks efficient (O(n))
- **Moonpool**: 7 comprehensive bug detectors using JSON-based system

The goal: Catch distributed correctness violations that single-actor assertions can't detect!

## Additional Resources

See separate reference files:
- `REFERENCE.md`: JSON schema for StateRegistry and check function API
- `EXAMPLES.md`: Per-peer message tracking, infrastructure event detection
