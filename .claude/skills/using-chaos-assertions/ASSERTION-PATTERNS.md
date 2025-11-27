# Assertion Patterns by Use Case

Common assertion patterns for different testing scenarios.

## Pattern 1: Request-Response Coverage

```rust
async fn send_request(&mut self, req: Request) -> Result<Response> {
    let timeout = Duration::from_secs(5);

    match self.time.timeout(timeout, self.transport.request(req)).await {
        Ok(Ok(response)) => {
            sometimes_assert!(
                request_completed,
                true,
                "Request completed successfully"
            );
            Ok(response)
        }
        Ok(Err(RequestError::NotFound)) => {
            sometimes_assert!(
                request_not_found,
                true,
                "Request returned NotFound error"
            );
            Err(RequestError::NotFound)
        }
        Ok(Err(e)) => {
            sometimes_assert!(
                request_other_error,
                true,
                "Request returned other error"
            );
            Err(e)
        }
        Err(_) => {
            sometimes_assert!(
                request_timeout,
                true,
                "Request timed out"
            );
            Err(RequestError::Timeout)
        }
    }
}
```

## Pattern 2: Connection State Validation

```rust
async fn manage_connection(&mut self, peer: PeerId) -> Result<()> {
    always_assert!(
        not_already_connected,
        !self.connections.contains_key(&peer),
        format!("Already connected to {:?}", peer)
    );

    let conn = self.connect(peer).await?;

    always_assert!(
        connection_stored,
        self.connections.contains_key(&peer),
        format!("Connection to {:?} not stored", peer)
    );

    sometimes_assert!(
        connection_established,
        true,
        "Connection successfully established"
    );

    Ok(())
}
```

## Pattern 3: Resource Pool Exhaustion

```rust
async fn acquire_from_pool(&mut self) -> Result<Resource> {
    if self.available.is_empty() && self.in_use.len() >= self.max_size {
        sometimes_assert!(
            pool_exhausted,
            true,
            "Resource pool exhausted"
        );
        return Err(PoolError::Exhausted);
    }

    let resource = self.allocate().await?;

    always_assert!(
        within_limit,
        self.in_use.len() <= self.max_size,
        format!("Pool exceeded max size: {} > {}", self.in_use.len(), self.max_size)
    );

    Ok(resource)
}
```

## Pattern 4: Message Ordering

```rust
async fn receive_message(&mut self) -> Result<Message> {
    let msg = self.transport.receive().await?;

    if msg.sequence_num <= self.last_sequence {
        sometimes_assert!(
            out_of_order_delivery,
            true,
            "Out-of-order message delivery detected"
        );
    }

    always_assert!(
        no_sequence_regression,
        msg.sequence_num > self.last_sequence - 100,  // Allow some reordering
        format!(
            "Severe sequence regression: current={}, last={}",
            msg.sequence_num, self.last_sequence
        )
    );

    self.last_sequence = msg.sequence_num;
    Ok(msg)
}
```

## Pattern 5: Actor Lifecycle

```rust
async fn deactivate_actor(&mut self, actor_id: ActorId) -> Result<()> {
    always_assert!(
        actor_exists,
        self.active_actors.contains_key(&actor_id),
        format!("Attempting to deactivate non-existent actor {:?}", actor_id)
    );

    self.active_actors.remove(&actor_id);

    always_assert!(
        actor_removed,
        !self.active_actors.contains_key(&actor_id),
        format!("Actor {:?} still active after deactivation", actor_id)
    );

    sometimes_assert!(
        deactivation_succeeded,
        true,
        "Actor deactivation succeeded"
    );

    Ok(())
}
```

## Pattern 6: Retry Logic

```rust
async fn operation_with_retries(&mut self) -> Result<Response> {
    for attempt in 0..self.max_retries {
        match self.try_operation().await {
            Ok(response) => {
                if attempt > 0 {
                    sometimes_assert!(
                        retry_succeeded,
                        true,
                        "Operation succeeded after retry"
                    );
                }
                return Ok(response);
            }
            Err(e) if attempt < self.max_retries - 1 => {
                sometimes_assert!(
                    retry_attempted,
                    true,
                    "Retry attempted after failure"
                );
                self.backoff(attempt).await;
            }
            Err(e) => {
                sometimes_assert!(
                    retries_exhausted,
                    true,
                    "All retries exhausted"
                );
                return Err(e);
            }
        }
    }

    unreachable!()
}
```

## Pattern 7: Load Conditions

```rust
async fn process_under_load(&mut self) {
    let queue_utilization = self.queue.len() as f64 / self.queue.capacity() as f64;

    if queue_utilization > 0.5 {
        sometimes_assert!(
            moderate_load,
            true,
            "System under moderate load (>50% queue utilization)"
        );
    }

    if queue_utilization > 0.9 {
        sometimes_assert!(
            high_load,
            true,
            "System under high load (>90% queue utilization)"
        );
    }

    always_assert!(
        no_overflow,
        self.queue.len() <= self.queue.capacity(),
        format!(
            "Queue overflow: {} > {}",
            self.queue.len(),
            self.queue.capacity()
        )
    );
}
```

## Pattern 8: Multi-Actor Coordination

```rust
async fn coordinate_actors(&mut self, actors: Vec<ActorId>) -> Result<()> {
    if actors.len() > 1 {
        sometimes_assert!(
            multi_actor_coordination,
            true,
            "Coordinating multiple actors"
        );
    }

    for actor in &actors {
        always_assert!(
            actor_unique,
            actors.iter().filter(|a| *a == actor).count() == 1,
            format!("Duplicate actor in coordination list: {:?}", actor)
        );
    }

    // ... coordinate

    sometimes_assert!(
        coordination_completed,
        true,
        "Actor coordination completed"
    );

    Ok(())
}
```

## Pattern 9: Network Partition Tolerance

```rust
async fn send_across_partition(&mut self, target: NodeId, msg: Message) -> Result<()> {
    if self.is_partitioned(target) {
        sometimes_assert!(
            partition_detected,
            true,
            "Network partition detected"
        );
        return Err(NetworkError::Partitioned);
    }

    match self.send(target, msg).await {
        Ok(()) => {
            sometimes_assert!(
                send_across_network,
                true,
                "Message sent across network"
            );
            Ok(())
        }
        Err(NetworkError::Unreachable) => {
            sometimes_assert!(
                network_unreachable,
                true,
                "Network unreachable error"
            );
            Err(NetworkError::Unreachable)
        }
        Err(e) => Err(e),
    }
}
```

## Pattern 10: State Machine Transitions

```rust
async fn transition(&mut self, new_state: State) -> Result<()> {
    let old_state = self.state;

    always_assert!(
        valid_transition,
        old_state.can_transition_to(new_state),
        format!("Invalid transition: {:?} -> {:?}", old_state, new_state)
    );

    self.state = new_state;

    sometimes_assert!(
        state_changed,
        old_state != new_state,
        "State actually changed during transition"
    );

    always_assert!(
        state_set,
        self.state == new_state,
        "State not properly set after transition"
    );

    Ok(())
}
```

## Summary: When to Use Each Pattern

| Pattern | Use sometimes_assert! For | Use always_assert! For |
|---------|-------------------------|----------------------|
| Request-Response | Success, timeout, specific errors | - |
| Connection State | Connection established | Not already connected, connection stored |
| Resource Pool | Pool exhausted | Within limit |
| Message Ordering | Out-of-order delivery | No severe regression |
| Actor Lifecycle | Deactivation succeeded | Actor exists, actor removed |
| Retry Logic | Retry attempted, exhausted | - |
| Load Conditions | Moderate/high load | No overflow |
| Coordination | Multi-actor scenario, completed | Actor uniqueness |
| Network Partition | Partition detected, unreachable | - |
| State Machine | State changed | Valid transition, state set |
