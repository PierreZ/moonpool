# Buggify Placement Guide

This guide provides a decision tree and detailed guidance for where to place buggify calls.

## Decision Tree

```
Is this code doing I/O or state transitions?
│
├─ NO → Skip buggify (pure computation doesn't benefit)
│
└─ YES → Does this path handle errors?
   │
   ├─ NO → Consider adding error handling first!
   │
   └─ YES → Is this error path well-tested?
      │
      ├─ YES → Low priority for buggify
      │
      └─ NO → ⭐ Add buggify here!
         │
         └─ Choose type:
            ├─ Network operation → Force failure
            ├─ State transition → Add delay
            ├─ Resource allocation → Shrink limits
            ├─ Timeout operation → Shrink timeout
            └─ Registration/Lookup → Force failure
```

## Placement Patterns by Code Structure

### Pattern 1: Around External Calls

**Where**: Before calls to network, directory, catalog, storage

**Why**: External dependencies can fail; test your handling

```rust
async fn call_external_service(&self) -> Result<Response> {
    // ⭐ Add buggify before external call
    if buggify!() {
        return Err(ServiceError::Unavailable);
    }

    self.external_service.call().await
}
```

### Pattern 2: In Resource Allocation

**Where**: When creating queues, buffers, connection pools

**Why**: Force resource pressure to test limits

```rust
fn allocate_resources(&self) -> Resources {
    let capacity = if buggify!() {
        2   // ⭐ Small capacity forces overflow
    } else {
        1000
    };

    Resources::with_capacity(capacity)
}
```

### Pattern 3: During State Transitions

**Where**: Between state changes (Idle → Active, Connecting → Connected)

**Why**: Widen race condition windows

```rust
async fn transition_to_active(&mut self) -> Result<()> {
    self.state = State::Activating;

    // ⭐ Delay widens race window
    if buggify!() {
        self.time.sleep(Duration::from_millis(50)).await;
    }

    self.state = State::Active;
    Ok(())
}
```

### Pattern 4: In Retry Loops

**Where**: Inside retry logic, backoff calculations

**Why**: Test retry exhaustion, backoff logic

```rust
async fn retry_operation(&mut self) -> Result<()> {
    let max_retries = if buggify!() {
        1  // ⭐ Force early exhaustion
    } else {
        5
    };

    for attempt in 0..max_retries {
        match self.try_operation().await {
            Ok(r) => return Ok(r),
            Err(_) if attempt < max_retries - 1 => {
                let backoff = if buggify!() {
                    Duration::from_millis(1)  // ⭐ Rapid retries
                } else {
                    Duration::from_secs(1)
                };
                self.time.sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }

    Err(Error::RetriesExhausted)
}
```

### Pattern 5: In Timeout Operations

**Where**: When calculating timeout durations

**Why**: Create time pressure, test timeout handling

```rust
async fn operation_with_timeout(&self) -> Result<Response> {
    let timeout = if buggify!() {
        Duration::from_millis(1)  // ⭐ Very short
    } else {
        Duration::from_secs(30)
    };

    self.time.timeout(timeout, self.long_operation()).await?
}
```

## Code Location Analysis

### Analyze: Network Layer

| Location | Priority | Reason |
|----------|----------|--------|
| `connect()` | **HIGH** | Connection failures are common in production |
| `send()` | **HIGH** | Send failures test retry/failover logic |
| `receive()` | **MEDIUM** | Receive failures less common but important |
| Socket options | **LOW** | Configuration rarely fails |

**Example**:
```rust
async fn connect(&mut self, addr: Address) -> Result<Connection> {
    // ⭐ HIGH priority - connection can fail
    if buggify!() {
        return Err(NetworkError::ConnectionRefused);
    }

    self.tcp_connect(addr).await
}
```

### Analyze: Actor Catalog

| Location | Priority | Reason |
|----------|----------|--------|
| `get_or_create()` race window | **HIGH** | Concurrent activation is tricky |
| Actor factory calls | **HIGH** | Creation can fail |
| State lookups | **MEDIUM** | Lookups usually succeed |
| Simple getters | **LOW** | No complex logic |

**Example**:
```rust
async fn get_or_create(&mut self, id: ActorId) -> Result<ActorHandle> {
    if let Some(handle) = self.actors.get(&id) {
        return Ok(handle.clone());
    }

    // ⭐ HIGH priority - widen race window
    if buggify!() {
        self.time.sleep(Duration::from_millis(50)).await;
    }

    // Check again
    if let Some(handle) = self.actors.get(&id) {
        return Ok(handle.clone());
    }

    // ⭐ HIGH priority - creation can fail
    if buggify!() {
        return Err(ActorError::CreationFailed);
    }

    self.create_new(id).await
}
```

### Analyze: Message Bus

| Location | Priority | Reason |
|----------|----------|--------|
| `route_message()` directory lookup | **HIGH** | Directory can be stale/wrong |
| Message serialization | **MEDIUM** | Serialization rarely fails |
| Queue operations | **HIGH** | Queues can overflow |
| Metrics collection | **LOW** | Metrics are non-critical |

**Example**:
```rust
async fn route_message(&mut self, target: ActorId, msg: Message) -> Result<()> {
    // ⭐ HIGH priority - directory lookup can fail
    let node = if buggify!() {
        return Err(RoutingError::DirectoryUnavailable);
    } else {
        self.directory.lookup(&target).await?
    };

    // ⭐ HIGH priority - queue can overflow
    if self.queue.len() >= if buggify!() { 2 } else { 1000 } {
        return Err(RoutingError::QueueFull);
    }

    self.send_to_node(node, target, msg).await
}
```

### Analyze: Directory Service

| Location | Priority | Reason |
|----------|----------|--------|
| `register()` | **HIGH** | Registration can conflict/fail |
| `lookup()` after cache miss | **MEDIUM** | Backend lookup can fail |
| `lookup()` from cache | **LOW** | Cache hits are reliable |
| `unregister()` | **MEDIUM** | Unregister can race with lookups |

**Example**:
```rust
async fn register(&mut self, actor: ActorId, node: NodeId) -> Result<()> {
    // ⭐ HIGH priority - registration can fail
    if buggify!() {
        return Err(DirectoryError::ConflictDetected);
    }

    // ⭐ HIGH priority - widen race window
    if buggify!() {
        self.time.sleep(Duration::from_millis(50)).await;
    }

    self.backend.store(actor, node).await
}
```

## Anti-Patterns: Where NOT to Add Buggify

### 1. Pure Computation

```rust
// ❌ Don't add buggify
fn calculate_hash(data: &[u8]) -> u64 {
    // Pure computation - no I/O, no state changes
    hash_function(data)
}
```

### 2. Trivial Getters

```rust
// ❌ Don't add buggify
fn get_node_id(&self) -> NodeId {
    self.node_id
}
```

### 3. Constructor/Initialization

```rust
// ❌ Don't add buggify in constructors
impl Actor {
    fn new(id: ActorId) -> Self {
        // Initialization should be reliable
        Self { id, state: State::New }
    }
}
```

### 4. Hot Paths Without Error Handling

```rust
// ❌ Don't add buggify without handling
async fn process_messages(&mut self) {
    loop {
        let msg = self.queue.pop().await;

        // If you add buggify here, where does error go?
        // Add error handling first!
        self.handle_message(msg).await;
    }
}
```

### 5. Already Chaotic Paths

```rust
// ❌ Don't add redundant buggify
async fn send_over_network(&mut self, msg: Message) -> Result<()> {
    // Network simulation already injects failures
    // Additional buggify is redundant
    self.network.send(msg).await
}
```

## Priority Matrix

| Code Location | I/O Risk | State Complexity | Race Potential | Priority |
|--------------|----------|------------------|----------------|----------|
| Network connect | High | Medium | Low | **HIGH** |
| Actor activation | Low | High | High | **HIGH** |
| Directory lookup | Medium | Medium | Medium | **HIGH** |
| Message routing | Medium | High | Medium | **HIGH** |
| Queue operations | Low | Medium | High | **HIGH** |
| State persistence | High | Medium | Low | **MEDIUM** |
| Configuration load | Medium | Low | Low | **MEDIUM** |
| Metrics collection | Low | Low | Low | **LOW** |
| Pure calculation | None | None | None | **SKIP** |

## Buggify Density Guidelines

Don't overload modules with too many buggify calls:

### Target Density

- **Network module**: 5-10 buggify sites
- **Actor catalog**: 3-5 buggify sites
- **Message bus**: 5-8 buggify sites
- **Directory**: 3-5 buggify sites
- **Per function**: Max 2-3 buggify sites

### Example: Balanced Buggify in Connection Logic

```rust
async fn establish_connection(&mut self, peer: PeerId) -> Result<Connection> {
    // Buggify 1: Connection failure
    if buggify!() {
        return Err(NetworkError::ConnectionRefused);
    }

    let mut backoff = Duration::from_millis(100);

    loop {
        match self.try_connect(peer).await {
            Ok(conn) => return Ok(conn),
            Err(_) => {
                // Buggify 2: Shrink backoff
                if buggify!() {
                    backoff = Duration::from_millis(1);
                }

                self.time.sleep(backoff).await;
                backoff = backoff * 2;

                // Buggify 3: Force early give up
                if buggify!() {
                    return Err(NetworkError::RetriesExhausted);
                }
            }
        }
    }
}

// Total: 3 buggify sites - GOOD
```

## Checklist for Each Buggify Site

Before adding buggify:

- [ ] Is this an I/O operation or state transition?
- [ ] Can this operation fail in production?
- [ ] Is there error handling for this failure?
- [ ] Will triggering this help find bugs?
- [ ] Is the probability appropriate (25% default, or tuned)?
- [ ] Have I added an assertion to track coverage?
- [ ] Have I documented the intent with a comment?
- [ ] Does this avoid hot loops?
- [ ] Is module density reasonable (<10 sites)?

## Advanced: Conditional Buggify

Combine buggify with system state for targeted chaos:

```rust
async fn process_request(&mut self, req: Request) -> Result<Response> {
    // Only inject chaos under load
    if self.queue.len() > 100 && buggify!() {
        return Err(OverloadError::TooManyRequests);
    }

    // Only inject chaos for specific actor types
    if req.target.is_virtual_actor() && buggify!() {
        return Err(RoutingError::VirtualActorNotFound);
    }

    // Only inject chaos during state transitions
    if self.state == State::Transitioning && buggify!() {
        self.time.sleep(Duration::from_millis(100)).await;
    }

    self.handle_request(req).await
}
```

## Review Checklist

When reviewing code with buggify:

- [ ] Buggify sites target error paths (not happy paths)
- [ ] Each site has clear purpose (documented)
- [ ] Probabilities are tuned appropriately
- [ ] Coverage is tracked with assertions
- [ ] Density is reasonable (not too many in one function)
- [ ] Error handling exists for all injected failures
- [ ] No buggify in hot loops or trivial functions

## Summary

**High Priority Locations**:
- Network connection/send failures
- Directory registration/lookup failures
- Actor activation race windows
- Queue overflow conditions
- Timeout operations

**Medium Priority Locations**:
- State persistence failures
- Retry/backoff logic
- Resource allocation

**Low Priority Locations**:
- Configuration loading
- Metrics collection
- Cached lookups

**Skip**:
- Pure computation
- Trivial getters
- Constructors
- Already chaotic paths
