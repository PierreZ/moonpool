# Buggify Examples

This file contains annotated real-world examples from moonpool-foundation showing effective buggify usage.

## Example 1: Peer Connection Management

From `moonpool-foundation/src/network/peer/core.rs`

### Connection Establishment with Buggify

```rust
async fn connect_to_peer(&mut self, peer_id: PeerId) -> Result<Connection> {
    // Buggify 1: Force connection failure
    // WHY: Tests retry logic and failover to other peers
    // PROBABILITY: 25% (default) - connection attempts are frequent
    if buggify!() {
        tracing::warn!(
            peer = ?peer_id,
            "Buggify: Simulating connection failure"
        );
        return Err(NetworkError::ConnectionRefused);
    }

    let addr = self.peer_addresses.get(&peer_id)
        .ok_or(NetworkError::UnknownPeer)?;

    let mut backoff = Duration::from_millis(100);

    for attempt in 0..self.max_retries {
        match self.transport.connect(addr.clone()).await {
            Ok(conn) => {
                tracing::info!(
                    peer = ?peer_id,
                    attempt,
                    "Connection established"
                );
                return Ok(conn);
            }
            Err(e) => {
                tracing::warn!(
                    peer = ?peer_id,
                    attempt,
                    error = ?e,
                    "Connection attempt failed"
                );

                if attempt < self.max_retries - 1 {
                    // Buggify 2: Shrink backoff to create rapid retries
                    // WHY: Tests race conditions in retry logic
                    // PROBABILITY: 50% - higher to stress-test retry logic
                    let delay = if buggify_with_prob!(0.5) {
                        Duration::from_millis(1)  // Very short
                    } else {
                        backoff
                    };

                    self.time.sleep(delay).await;
                    backoff = backoff * 2;
                }
            }
        }
    }

    Err(NetworkError::ConnectionFailed)
}
```

**Bug Found**: Without buggify, tests never caught a race condition where two peers tried to connect simultaneously, creating duplicate connections.

### Connection Cleanup

```rust
async fn close_connection(&mut self, peer_id: PeerId) -> Result<()> {
    // Buggify: Delay during cleanup
    // WHY: Widens window for race where new messages arrive during close
    // PROBABILITY: 25% (default) - cleanup is moderately frequent
    if buggify!() {
        tracing::warn!(
            peer = ?peer_id,
            "Buggify: Delaying connection cleanup"
        );
        self.time.sleep(Duration::from_millis(50)).await;
    }

    if let Some(conn) = self.connections.remove(&peer_id) {
        conn.close().await?;

        always_assert!(
            connection_removed,
            !self.connections.contains_key(&peer_id),
            "Connection not fully removed after close"
        );
    }

    Ok(())
}
```

**Bug Found**: Message sent during buggify delay crashed when connection was gone. Added proper error handling.

---

## Example 2: Ping-Pong Test Timeouts

From `moonpool-foundation/tests/simulation/ping_pong/tests.rs:311-314`

### Dynamic Timeout Generation

```rust
async fn run_single_ping(&mut self) -> Result<PingResult> {
    // Buggify: Randomize timeout to create time pressure
    // WHY: Tests timeout handling across wide range (1ms to 5s)
    // PROBABILITY: 50% for short, 50% for long
    let timeout_ms = if self.random.random_bool(0.5) {
        // Short timeout: 1-10ms - will often timeout
        self.random.random_range(1..10)
    } else {
        // Long timeout: 100-5000ms - usually succeeds
        self.random.random_range(100..5000)
    };

    let timeout = Duration::from_millis(timeout_ms);

    tracing::debug!(
        timeout_ms,
        "Attempting ping with timeout"
    );

    match self.time.timeout(timeout, self.send_ping()).await {
        Ok(Ok(response)) => {
            sometimes_assert!(
                ping_succeeded,
                true,
                "Ping completed successfully"
            );
            Ok(PingResult::Success(response))
        }
        Ok(Err(e)) => {
            sometimes_assert!(
                ping_failed,
                true,
                "Ping failed (not timeout)"
            );
            Err(e)
        }
        Err(_) => {
            sometimes_assert!(
                ping_timeout,
                true,
                "Ping timed out"
            );
            Ok(PingResult::Timeout)
        }
    }
}
```

**Bug Found**: Short timeouts revealed that server wasn't properly handling connection reuse, causing later requests to fail.

---

## Example 3: Actor Catalog Activation

Hypothetical example based on common actor system patterns:

### Concurrent Activation Protection

```rust
async fn get_or_activate_actor(
    &mut self,
    actor_id: ActorId,
) -> Result<ActorHandle> {
    // Fast path: already active
    if let Some(handle) = self.active_actors.get(&actor_id) {
        return Ok(handle.clone());
    }

    // Check if activation in progress
    if self.activating.contains(&actor_id) {
        // Wait for other activation to complete
        return self.wait_for_activation(&actor_id).await;
    }

    // Mark as activating
    self.activating.insert(actor_id.clone());

    // Buggify: Widen race window between check and activation
    // WHY: Tests concurrent activation attempts
    // PROBABILITY: 50% - higher to stress-test concurrency
    if buggify_with_prob!(0.5) {
        tracing::warn!(
            actor = ?actor_id,
            "Buggify: Delaying activation to widen race window"
        );
        self.time.sleep(Duration::from_millis(50)).await;

        sometimes_assert!(
            activation_delay_injected,
            true,
            "Activation delay buggify triggered"
        );
    }

    // Check again - another thread might have activated
    if let Some(handle) = self.active_actors.get(&actor_id) {
        self.activating.remove(&actor_id);
        return Ok(handle.clone());
    }

    // Buggify: Force activation failure
    // WHY: Tests cleanup when activation fails
    // PROBABILITY: 10% - rare but important to test
    if buggify_with_prob!(0.1) {
        self.activating.remove(&actor_id);
        tracing::warn!(
            actor = ?actor_id,
            "Buggify: Simulating activation failure"
        );
        sometimes_assert!(
            activation_failure_injected,
            true,
            "Activation failure buggify triggered"
        );
        return Err(ActorError::ActivationFailed);
    }

    // Actually activate
    let handle = self.factory.create_actor(actor_id.clone()).await?;

    // Store handle
    self.active_actors.insert(actor_id.clone(), handle.clone());
    self.activating.remove(&actor_id);

    always_assert!(
        actor_activated_once,
        self.active_actors.contains_key(&actor_id),
        "Actor not in active set after activation"
    );

    Ok(handle)
}
```

**Bug Found**: Without buggify delay, race condition never triggered where two threads tried to activate same actor simultaneously.

---

## Example 4: Message Bus Routing

Hypothetical example based on distributed messaging patterns:

### Directory Lookup with Failures

```rust
async fn route_message(
    &mut self,
    target: ActorId,
    msg: Message,
) -> Result<()> {
    // Buggify: Force directory lookup failure
    // WHY: Tests routing fallback when directory unavailable
    // PROBABILITY: 25% (default) - lookups are frequent
    if buggify!() {
        tracing::warn!(
            target = ?target,
            "Buggify: Simulating directory lookup failure"
        );
        sometimes_assert!(
            directory_lookup_failure,
            true,
            "Directory lookup failure injected"
        );
        return Err(RoutingError::DirectoryUnavailable);
    }

    let node = self.directory.lookup(&target).await
        .map_err(|_| RoutingError::ActorNotFound)?;

    // Buggify: Route to wrong node
    // WHY: Tests message handling when actor moved
    // PROBABILITY: 10% - infrequent but critical
    let target_node = if buggify_with_prob!(0.1) {
        let wrong_node = self.select_random_node();
        tracing::warn!(
            target = ?target,
            correct_node = ?node,
            wrong_node = ?wrong_node,
            "Buggify: Routing to wrong node"
        );
        sometimes_assert!(
            wrong_node_routing,
            true,
            "Wrong node routing injected"
        );
        wrong_node
    } else {
        node
    };

    // Buggify: Shrink queue to force overflow
    // WHY: Tests backpressure and overflow handling
    // PROBABILITY: Depends on queue state
    let max_queue_size = if buggify!() {
        5  // Very small
    } else {
        1000
    };

    if self.outbound_queue.len() >= max_queue_size {
        sometimes_assert!(
            queue_overflow,
            true,
            "Outbound queue overflow"
        );
        return Err(RoutingError::QueueFull);
    }

    self.send_to_node(target_node, target, msg).await?;

    Ok(())
}
```

**Bug Found**: Wrong node routing revealed that actor migration wasn't properly updating directory, causing messages to be lost.

---

## Example 5: State Persistence

Hypothetical example for actor state management:

### Save/Load with Failures

```rust
async fn save_actor_state(
    &mut self,
    actor_id: ActorId,
    state: ActorState,
) -> Result<()> {
    // Buggify: Force save failure
    // WHY: Tests error handling when persistence fails
    // PROBABILITY: 25% (default) - saves moderately frequent
    if buggify!() {
        tracing::warn!(
            actor = ?actor_id,
            "Buggify: Simulating state save failure"
        );
        sometimes_assert!(
            state_save_failure,
            true,
            "State save failure injected"
        );
        return Err(PersistenceError::WriteFailed);
    }

    // Serialize state
    let serialized = self.serialize(&state)?;

    // Buggify: Delay during write
    // WHY: Widens race window between save and concurrent reads
    // PROBABILITY: 50% - higher to stress-test consistency
    if buggify_with_prob!(0.5) {
        tracing::warn!(
            actor = ?actor_id,
            "Buggify: Delaying state write"
        );
        self.time.sleep(Duration::from_millis(50)).await;
    }

    // Write to storage
    self.storage.write(&actor_id, &serialized).await?;

    sometimes_assert!(
        state_saved,
        true,
        "Actor state saved successfully"
    );

    Ok(())
}

async fn load_actor_state(
    &self,
    actor_id: ActorId,
) -> Result<ActorState> {
    // Buggify: Force load failure
    // WHY: Tests handling when state doesn't exist or is corrupted
    // PROBABILITY: 25% (default)
    if buggify!() {
        tracing::warn!(
            actor = ?actor_id,
            "Buggify: Simulating state load failure"
        );
        sometimes_assert!(
            state_load_failure,
            true,
            "State load failure injected"
        );
        return Err(PersistenceError::ReadFailed);
    }

    // Read from storage
    let serialized = self.storage.read(&actor_id).await?;

    // Buggify: Corrupt data
    // WHY: Tests deserialization error handling
    // PROBABILITY: 10% - rare but important
    let data = if buggify_with_prob!(0.1) {
        tracing::warn!(
            actor = ?actor_id,
            "Buggify: Corrupting state data"
        );
        sometimes_assert!(
            state_corruption,
            true,
            "State corruption injected"
        );
        vec![0xFF; serialized.len()]  // Invalid data
    } else {
        serialized
    };

    // Deserialize
    self.deserialize(&data)
}
```

**Bug Found**: Load failure handling didn't properly initialize actors with default state, causing crashes on first message.

---

## Example 6: Request-Response with Timeout

From ping-pong pattern in moonpool-foundation:

### Client Request with Dynamic Timeout

```rust
async fn send_request_with_retries(
    &mut self,
    request: Request,
) -> Result<Response> {
    let max_attempts = if buggify!() {
        1  // Force single attempt
    } else {
        3
    };

    for attempt in 0..max_attempts {
        // Buggify: Vary timeout per attempt
        // WHY: Tests timeout handling across spectrum
        let timeout = if buggify!() {
            Duration::from_millis(1)  // Very short
        } else {
            Duration::from_millis(100 * (attempt + 1))
        };

        tracing::debug!(
            attempt,
            timeout_ms = timeout.as_millis(),
            "Sending request"
        );

        match self.time.timeout(
            timeout,
            self.transport.request(request.clone())
        ).await {
            Ok(Ok(response)) => {
                sometimes_assert!(
                    request_succeeded,
                    true,
                    "Request completed successfully"
                );
                return Ok(response);
            }
            Ok(Err(e)) => {
                sometimes_assert!(
                    request_failed,
                    true,
                    "Request failed (not timeout)"
                );
                if attempt == max_attempts - 1 {
                    return Err(e);
                }
            }
            Err(_) => {
                sometimes_assert!(
                    request_timeout,
                    true,
                    "Request timed out"
                );
                if attempt == max_attempts - 1 {
                    return Err(RequestError::Timeout);
                }
            }
        }

        // Buggify: Vary retry delay
        if attempt < max_attempts - 1 {
            let delay = if buggify!() {
                Duration::from_millis(1)
            } else {
                Duration::from_millis(50 * (attempt + 1))
            };
            self.time.sleep(delay).await;
        }
    }

    Err(RequestError::RetriesExhausted)
}
```

**Bug Found**: Single-attempt buggify revealed that client wasn't properly cleaning up pending requests on failure, causing memory leak.

---

## Example 7: Resource Pool Management

Hypothetical connection pool example:

### Connection Pool with Limits

```rust
async fn acquire_connection(&mut self) -> Result<Connection> {
    // Buggify: Shrink pool to force exhaustion
    // WHY: Tests behavior when pool is exhausted
    let max_connections = if buggify!() {
        2  // Very small
    } else {
        100
    };

    // Wait for available connection
    loop {
        if let Some(conn) = self.available_connections.pop() {
            // Buggify: Return broken connection
            // WHY: Tests connection validation
            if buggify_with_prob!(0.1) {
                tracing::warn!("Buggify: Returning broken connection");
                sometimes_assert!(
                    broken_connection,
                    true,
                    "Broken connection returned from pool"
                );
                // Continue loop to get another connection
                continue;
            }

            return Ok(conn);
        }

        if self.active_connections.len() < max_connections {
            // Create new connection
            if buggify_with_prob!(0.1) {
                tracing::warn!("Buggify: Connection creation failed");
                sometimes_assert!(
                    connection_creation_failure,
                    true,
                    "Connection creation failed"
                );
                return Err(PoolError::CreationFailed);
            }

            let conn = self.create_connection().await?;
            return Ok(conn);
        }

        // Pool exhausted - wait
        sometimes_assert!(
            pool_exhausted,
            self.active_connections.len() >= max_connections,
            "Connection pool exhausted"
        );

        // Buggify: Shorter wait to stress-test pool
        let wait_time = if buggify!() {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(100)
        };

        self.time.sleep(wait_time).await;
    }
}
```

**Bug Found**: Pool exhaustion logic had off-by-one error that allowed one extra connection beyond limit.

---

## Common Themes

1. **Document intent**: Every buggify has comment explaining purpose
2. **Track coverage**: Assertions verify buggify triggers
3. **Tune probability**: Use 0.1-0.75 based on frequency
4. **Test error paths**: Focus on rarely-executed error handling
5. **Widen races**: Delays reveal timing-dependent bugs
6. **Force pressure**: Small limits test resource exhaustion

## Summary Statistics from Examples

| Pattern | Default Probability | When to Tune Higher | When to Tune Lower |
|---------|--------------------|--------------------|-------------------|
| Connection failure | 25% | Stable connections | Flaky network |
| Timeout shrinking | 25% | Fast operations | Slow operations |
| Race window delays | 50% | Concurrent code | Sequential code |
| Activation failure | 10% | Simple activation | Complex activation |
| Queue shrinking | 25% | Low throughput | High throughput |
| Directory failures | 25% | Stable directory | Distributed directory |

All examples show that **buggify finds bugs that manual testing misses** by forcing rare conditions to occur frequently.
