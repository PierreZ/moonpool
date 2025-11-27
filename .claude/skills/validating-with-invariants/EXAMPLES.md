# Invariant Examples

Real-world and practical examples of invariant checks.

## Example 1: Message Conservation

From moonpool-foundation's per-peer message tracking.

```rust
fn check_message_conservation(registry: &StateRegistry) -> Result<(), String> {
    let mut total_sent: u64 = 0;
    let mut total_received: u64 = 0;
    let mut total_in_flight: u64 = 0;

    // Aggregate across all nodes
    for (node_id, state) in registry.get_all() {
        let sent = state["metrics"]["messages_sent"]
            .as_u64()
            .unwrap_or(0);
        let received = state["metrics"]["messages_received"]
            .as_u64()
            .unwrap_or(0);
        let in_flight = state["metrics"]["messages_in_flight"]
            .as_u64()
            .unwrap_or(0);

        total_sent += sent;
        total_received += received;
        total_in_flight += in_flight;
    }

    // Conservation law: sent = received + in_flight + dropped
    // Therefore: received + in_flight ≤ sent
    if total_received + total_in_flight > total_sent {
        return Err(format!(
            "Message conservation violated: \
             sent={}, received={}, in_flight={}, \
             received+in_flight={} > sent",
            total_sent, total_received, total_in_flight,
            total_received + total_in_flight
        ));
    }

    Ok(())
}
```

**What It Catches**: Messages appearing from nowhere (duplication bugs)

---

## Example 2: Per-Peer Message Accounting

Detailed tracking per peer pair for routing bugs.

```rust
fn check_per_peer_accounting(registry: &StateRegistry) -> Result<(), String> {
    // Extract all peer-to-peer metrics
    let mut peer_pairs: HashMap<(String, String), (u64, u64)> = HashMap::new();

    for (node_id, state) in registry.get_all() {
        if let Some(peers) = state["per_peer_metrics"].as_object() {
            for (peer_id, metrics) in peers {
                let sent = metrics["sent_to_peer"].as_u64().unwrap_or(0);
                let received = metrics["received_from_peer"].as_u64().unwrap_or(0);

                // Track A → B
                let entry = peer_pairs
                    .entry((node_id.clone(), peer_id.clone()))
                    .or_insert((0, 0));
                entry.0 += sent;

                // Track B ← A (reverse)
                let reverse_entry = peer_pairs
                    .entry((peer_id.clone(), node_id.clone()))
                    .or_insert((0, 0));
                reverse_entry.1 += received;
            }
        }
    }

    // Validate: For each A → B, sent_by_A ≥ received_by_B
    for ((from, to), (sent, received)) in &peer_pairs {
        if *received > *sent {
            return Err(format!(
                "Per-peer conservation violated: {} → {}: \
                 sent={}, received={}",
                from, to, sent, received
            ));
        }
    }

    Ok(())
}
```

**What It Catches**: Routing bugs where messages are misdelivered or duplicated between specific peer pairs

---

## Example 3: Single Location per Actor

Directory consistency invariant.

```rust
fn check_single_location(registry: &StateRegistry) -> Result<(), String> {
    let mut actor_locations: HashMap<String, Vec<String>> = HashMap::new();

    // Collect all directory entries
    for (node_id, state) in registry.get_all() {
        if let Some(directory) = state["directory_entries"].as_array() {
            for entry in directory {
                let actor_id = entry["actor_id"]
                    .as_str()
                    .ok_or("Missing actor_id")?
                    .to_string();

                actor_locations
                    .entry(actor_id)
                    .or_default()
                    .push(node_id.clone());
            }
        }
    }

    // Validate: Each actor in at most one location
    for (actor_id, locations) in &actor_locations {
        if locations.len() > 1 {
            return Err(format!(
                "Actor {} in multiple locations: {:?}",
                actor_id, locations
            ));
        }
    }

    Ok(())
}
```

**What It Catches**: Directory inconsistency where actor appears in multiple locations

---

## Example 4: Resource Leak Detection

Track resource allocation/deallocation.

```rust
fn check_no_resource_leaks(registry: &StateRegistry) -> Result<(), String> {
    let mut total_allocated: u64 = 0;
    let mut total_freed: u64 = 0;
    let mut total_held: u64 = 0;

    for (node_id, state) in registry.get_all() {
        let allocated = state["resources"]["allocated"]
            .as_u64()
            .unwrap_or(0);
        let freed = state["resources"]["freed"]
            .as_u64()
            .unwrap_or(0);
        let held = state["resources"]["held"]
            .as_u64()
            .unwrap_or(0);

        total_allocated += allocated;
        total_freed += freed;
        total_held += held;

        // Per-actor check
        if held != allocated - freed {
            return Err(format!(
                "Resource accounting error in {}: \
                 held={}, allocated={}, freed={}, \
                 expected held={}",
                node_id, held, allocated, freed, allocated - freed
            ));
        }
    }

    // Global check
    if total_held != total_allocated - total_freed {
        return Err(format!(
            "Global resource leak: \
             held={}, allocated={}, freed={}",
            total_held, total_allocated, total_freed
        ));
    }

    Ok(())
}
```

**What It Catches**: Resource leaks, double-frees, accounting bugs

---

## Example 5: Queue Bounds

Ensure queues don't exceed capacity.

```rust
fn check_queue_bounds(registry: &StateRegistry) -> Result<(), String> {
    for (node_id, state) in registry.get_all() {
        if let Some(queues) = state["queues"].as_object() {
            for (queue_name, queue_state) in queues {
                let length = queue_state["length"]
                    .as_u64()
                    .unwrap_or(0);
                let capacity = queue_state["capacity"]
                    .as_u64()
                    .unwrap_or(u64::MAX);

                if length > capacity {
                    return Err(format!(
                        "Queue overflow in {} (queue: {}): \
                         length={} > capacity={}",
                        node_id, queue_name, length, capacity
                    ));
                }
            }
        }
    }

    Ok(())
}
```

**What It Catches**: Queue overflow bugs, capacity violations

---

## Example 6: Connection Count Consistency

Track connection state across peers.

```rust
fn check_connection_consistency(registry: &StateRegistry) -> Result<(), String> {
    let mut connection_pairs: HashMap<(String, String), usize> = HashMap::new();

    // Count connections from each node's perspective
    for (node_id, state) in registry.get_all() {
        if let Some(connections) = state["connections"].as_array() {
            for conn in connections {
                let peer_id = conn["peer_id"]
                    .as_str()
                    .ok_or("Missing peer_id")?;

                let pair = if node_id < peer_id {
                    (node_id.clone(), peer_id.to_string())
                } else {
                    (peer_id.to_string(), node_id.clone())
                };

                *connection_pairs.entry(pair).or_insert(0) += 1;
            }
        }
    }

    // Each connection should be seen from both sides (count = 2)
    for ((node1, node2), count) in &connection_pairs {
        if *count == 1 {
            return Err(format!(
                "Asymmetric connection: {} ↔ {} (count={})",
                node1, node2, count
            ));
        } else if *count > 2 {
            return Err(format!(
                "Duplicate connection: {} ↔ {} (count={})",
                node1, node2, count
            ));
        }
    }

    Ok(())
}
```

**What It Catches**: Half-open connections, duplicate connections

---

## Example 7: State Machine Validity

Ensure all actors in valid states.

```rust
fn check_valid_states(registry: &StateRegistry) -> Result<(), String> {
    let valid_states = ["idle", "active", "transitioning", "deactivating"];

    for (actor_id, state) in registry.get_all() {
        if let Some(actor_state) = state["state"]["status"].as_str() {
            if !valid_states.contains(&actor_state) {
                return Err(format!(
                    "Invalid state for {}: '{}'",
                    actor_id, actor_state
                ));
            }

            // State-specific invariants
            match actor_state {
                "active" => {
                    // Active actors must have resources
                    let has_resources = state["resources"]["held"]
                        .as_u64()
                        .unwrap_or(0) > 0;

                    if !has_resources {
                        return Err(format!(
                            "Active actor {} has no resources",
                            actor_id
                        ));
                    }
                }
                "idle" => {
                    // Idle actors shouldn't have pending work
                    let pending = state["state"]["pending_work"]
                        .as_u64()
                        .unwrap_or(0);

                    if pending > 0 {
                        return Err(format!(
                            "Idle actor {} has pending work: {}",
                            actor_id, pending
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}
```

**What It Catches**: Invalid state transitions, state inconsistencies

---

## Example 8: Banking System Money Conservation

Distributed transaction correctness.

```rust
fn check_money_conservation(registry: &StateRegistry) -> Result<(), String> {
    let mut total_balance: i64 = 0;
    let mut total_deposits: i64 = 0;
    let mut total_withdrawals: i64 = 0;

    // Get initial capital from config
    let initial_capital = registry.get("system_config")
        .and_then(|c| c["initial_capital"].as_i64())
        .ok_or("Missing initial_capital")?;

    for (account_id, state) in registry.get_all() {
        if state["type"] == "bank_account" {
            let balance = state["balance"]
                .as_i64()
                .ok_or(format!("Missing balance for {}", account_id))?;
            let deposits = state["total_deposits"]
                .as_i64()
                .unwrap_or(0);
            let withdrawals = state["total_withdrawals"]
                .as_i64()
                .unwrap_or(0);

            // Per-account check
            if balance < 0 {
                return Err(format!(
                    "Negative balance in {}: {}",
                    account_id, balance
                ));
            }

            total_balance += balance;
            total_deposits += deposits;
            total_withdrawals += withdrawals;
        }
    }

    // Global money conservation
    let expected = initial_capital + total_deposits - total_withdrawals;
    if total_balance != expected {
        return Err(format!(
            "Money conservation violated: \
             current_balance={}, expected={}, \
             initial={}, deposits={}, withdrawals={}",
            total_balance, expected, initial_capital,
            total_deposits, total_withdrawals
        ));
    }

    Ok(())
}
```

**What It Catches**: Money creation/destruction bugs, accounting errors

---

## Complex Example: Infrastructure Event Detection

From moonpool-foundation to prevent infinite simulation loops.

```rust
fn check_for_livelock(registry: &StateRegistry) -> Result<(), String> {
    let mut has_application_work = false;

    for (node_id, state) in registry.get_all() {
        // Check for pending application-level work
        let pending_requests = state["metrics"]["pending_requests"]
            .as_u64()
            .unwrap_or(0);
        let queue_length = state["state"]["queue_length"]
            .as_u64()
            .unwrap_or(0);
        let active_tasks = state["state"]["active_tasks"]
            .as_u64()
            .unwrap_or(0);

        if pending_requests > 0 || queue_length > 0 || active_tasks > 0 {
            has_application_work = true;
            break;
        }
    }

    // Check if only infrastructure events remain
    if !has_application_work {
        let infrastructure_events = registry.get("simulator")
            .and_then(|s| s["infrastructure_events"].as_u64())
            .unwrap_or(0);

        if infrastructure_events > 100 {
            // Only infrastructure events for extended period → potential livelock
            return Err(format!(
                "Possible livelock: no application work but {} infrastructure events",
                infrastructure_events
            ));
        }
    }

    Ok(())
}
```

**What It Catches**: Infinite loops where only background work continues (connection retries, timeouts) but no application progress

---

## Summary: Invariant Patterns

| Pattern | Example | What It Catches |
|---------|---------|-----------------|
| Conservation Law | Message conservation | Duplication, creation from nothing |
| Per-Entity Accounting | Per-peer tracking | Routing bugs, misdelivery |
| Uniqueness | Single location | Directory inconsistency |
| Resource Tracking | Leak detection | Memory/resource leaks |
| Bounds Checking | Queue capacity | Overflow bugs |
| Symmetry | Connection pairs | Half-open connections |
| State Validity | State machine | Invalid transitions |
| Domain Invariants | Money conservation | Business logic errors |
| Liveness | Livelock detection | Infinite background loops |

All examples follow the pattern:
1. Aggregate relevant state from registry
2. Compute global property
3. Return `Err` if violated with detailed message
