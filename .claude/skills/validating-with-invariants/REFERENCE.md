# Invariant System Reference

API reference for implementing the JSON-based invariant system.

## StateRegistry API

### Core Structure

```rust
pub struct StateRegistry {
    // Actor ID -> JSON state
    states: HashMap<String, serde_json::Value>,
}
```

### Methods

#### register

```rust
pub fn register(&mut self, actor_id: String, state: serde_json::Value)
```

Register or update an actor's state.

**Example**:
```rust
let state = json!({
    "messages_sent": 42,
    "messages_received": 38,
    "queue_length": 5
});

registry.register("actor_1".to_string(), state);
```

#### get

```rust
pub fn get(&self, actor_id: &str) -> Option<&serde_json::Value>
```

Retrieve a specific actor's state.

**Example**:
```rust
if let Some(state) = registry.get("actor_1") {
    let queue_len = state["queue_length"].as_u64().unwrap_or(0);
}
```

#### get_all

```rust
pub fn get_all(&self) -> &HashMap<String, serde_json::Value>
```

Get all registered states for iteration.

**Example**:
```rust
for (actor_id, state) in registry.get_all() {
    let sent = state["messages_sent"].as_u64().unwrap_or(0);
    total_sent += sent;
}
```

#### remove

```rust
pub fn remove(&mut self, actor_id: &str) -> Option<serde_json::Value>
```

Remove an actor's state (e.g., when actor deactivates).

**Example**:
```rust
if let Some(old_state) = registry.remove("actor_1") {
    // Clean up
}
```

## InvariantCheck Function Type

### Signature

```rust
type InvariantCheck = fn(&StateRegistry) -> Result<(), String>;
```

### Contract

**Input**: Immutable reference to StateRegistry
**Output**: `Ok(())` if invariant holds, `Err(message)` if violated

### Template

```rust
fn check_my_invariant(registry: &StateRegistry) -> Result<(), String> {
    // 1. Extract relevant state
    let states = registry.get_all();

    // 2. Compute property
    let property_holds = /* ... compute ... */;

    // 3. Validate
    if !property_holds {
        return Err(format!("Invariant violated: ..."));
    }

    Ok(())
}
```

## JSON State Schema Conventions

### Recommended Structure

```json
{
    "actor_id": "unique_identifier",
    "type": "actor_type",

    "metrics": {
        "messages_sent": 42,
        "messages_received": 38,
        "requests_pending": 3
    },

    "state": {
        "status": "active",
        "connections": 2,
        "queue_length": 5,
        "queue_capacity": 100
    },

    "resources": {
        "allocated": 10,
        "freed": 5,
        "held": 5
    }
}
```

### Field Types

- **Counters**: Use `u64` for metrics (messages, bytes, etc.)
- **States**: Use `String` for enums (`"active"`, `"idle"`, etc.)
- **Collections**: Use arrays for lists
- **Nested**: Group related fields

### Example: Actor State

```rust
let state = json!({
    "actor_id": format!("actor_{}", id),
    "type": "message_bus",

    "metrics": {
        "messages_sent": self.metrics.sent,
        "messages_received": self.metrics.received,
        "messages_in_flight": self.metrics.in_flight,
    },

    "state": {
        "connections": self.connections.len(),
        "queue_length": self.queue.len(),
        "queue_capacity": self.queue.capacity(),
    }
});

registry.register(actor_id, state);
```

## Invariant Registration

### Manual Registration

```rust
let mut invariant_checks: Vec<InvariantCheck> = vec![
    check_message_conservation,
    check_single_location,
    check_resource_bounds,
];

// Run after every simulation event
for check in &invariant_checks {
    if let Err(msg) = check(&state_registry) {
        panic!("Invariant violation: {}", msg);
    }
}
```

### Framework Integration

```rust
pub struct SimulationWorld {
    state_registry: StateRegistry,
    invariant_checks: Vec<InvariantCheck>,
}

impl SimulationWorld {
    fn validate_invariants(&self) -> Result<(), String> {
        for check in &self.invariant_checks {
            check(&self.state_registry)?;
        }
        Ok(())
    }

    async fn process_event(&mut self, event: Event) {
        // Process event
        self.handle_event(event).await;

        // Update state registry
        for actor in &self.actors {
            let state = actor.export_state();
            self.state_registry.register(actor.id(), state);
        }

        // Validate invariants
        if let Err(msg) = self.validate_invariants() {
            panic!("Invariant violation after event: {}", msg);
        }
    }
}
```

## Common Helper Functions

### Aggregate Counters

```rust
fn sum_field(registry: &StateRegistry, field_path: &str) -> u64 {
    registry.get_all()
        .values()
        .filter_map(|state| {
            field_path.split('.')
                .fold(Some(state), |acc, key| {
                    acc.and_then(|v| v.get(key))
                })
                .and_then(|v| v.as_u64())
        })
        .sum()
}

// Usage:
let total_sent = sum_field(&registry, "metrics.messages_sent");
```

### Collect Values

```rust
fn collect_field<T>(
    registry: &StateRegistry,
    field_path: &str,
    extractor: impl Fn(&serde_json::Value) -> Option<T>
) -> Vec<T> {
    registry.get_all()
        .values()
        .filter_map(|state| {
            field_path.split('.')
                .fold(Some(state), |acc, key| {
                    acc.and_then(|v| v.get(key))
                })
                .and_then(&extractor)
        })
        .collect()
}

// Usage:
let all_queues: Vec<u64> = collect_field(
    &registry,
    "state.queue_length",
    |v| v.as_u64()
);
```

### Filter States

```rust
fn filter_by_type<'a>(
    registry: &'a StateRegistry,
    type_name: &str
) -> impl Iterator<Item = (&'a String, &'a serde_json::Value)> {
    registry.get_all()
        .iter()
        .filter(move |(_, state)| {
            state.get("type")
                .and_then(|v| v.as_str())
                .map(|t| t == type_name)
                .unwrap_or(false)
        })
}

// Usage:
for (id, state) in filter_by_type(&registry, "message_bus") {
    // Check message bus specific invariants
}
```

## Error Message Guidelines

### Good Error Messages

```rust
// ✅ Specific and actionable
return Err(format!(
    "Message conservation violated: \
     total_sent={}, total_received={}, difference={}. \
     Actor '{}' received {} but sent {}",
    total_sent, total_received, total_received - total_sent,
    problem_actor, actor_received, actor_sent
));
```

### Poor Error Messages

```rust
// ❌ Vague and unhelpful
return Err("Invariant failed".to_string());

// ❌ Missing context
return Err(format!("Count mismatch: {} != {}", a, b));
```

## Performance Tips

### Use Early Returns

```rust
fn check_invariant(registry: &StateRegistry) -> Result<(), String> {
    // Fail fast on first violation
    for (id, state) in registry.get_all() {
        if violates_property(state) {
            return Err(format!("Violation in {}", id));
        }
    }
    Ok(())
}
```

### Cache Expensive Computations

```rust
fn check_expensive_invariant(registry: &StateRegistry) -> Result<(), String> {
    // Compute once, use multiple times
    let total = compute_expensive_total(registry);

    for (id, state) in registry.get_all() {
        if !validate_against_total(state, total) {
            return Err(format!("Violation in {}", id));
        }
    }

    Ok(())
}
```

### Avoid Nested Iterations

```rust
// ❌ O(n²) - avoid if possible
for (id1, state1) in registry.get_all() {
    for (id2, state2) in registry.get_all() {
        // ...
    }
}

// ✅ O(n) - preferred
let mut seen = HashSet::new();
for (id, state) in registry.get_all() {
    if !seen.insert(extract_key(state)) {
        return Err(format!("Duplicate in {}", id));
    }
}
```

## Testing Invariants

### Unit Test Template

```rust
#[test]
fn test_invariant_detects_violation() {
    let mut registry = StateRegistry::new();

    // Setup valid state
    registry.register("actor_1".to_string(), json!({
        "messages_sent": 10,
        "messages_received": 8
    }));

    // Should pass
    assert!(check_message_conservation(&registry).is_ok());

    // Introduce violation
    registry.register("actor_2".to_string(), json!({
        "messages_sent": 5,
        "messages_received": 10  // More received than sent!
    }));

    // Should fail
    assert!(check_message_conservation(&registry).is_err());
}
```

### Integration Test

```rust
#[test]
fn slow_simulation_with_invariants() {
    let mut checks: Vec<InvariantCheck> = vec![
        check_message_conservation,
        check_single_location,
    ];

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .unwrap();

    runtime.block_on(async {
        let report = SimulationBuilder::new()
            .use_random_config()
            .register_invariant_checks(checks)
            .register_workload("test", workload)
            .run()
            .await;

        // Invariants validated throughout execution
        assert!(report.invariant_violations.is_empty());
    });
}
```

## Summary

**Core Components**:
- StateRegistry: Holds actor states as JSON
- InvariantCheck: Functions validating global properties
- JSON schema: Standardized state structure

**Best Practices**:
- Efficient checks (O(n) preferred)
- Specific error messages
- Early returns on violations
- Test both valid and invalid states
