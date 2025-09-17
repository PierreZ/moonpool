# Transport Layer Split Plan: Rebuilding pz/peers Branch Incrementally

## Overview

This document contains the complete knowledge from the `pz/peers` branch to rebuild the Sans I/O transport layer incrementally with proper chaos testing enabled from the start. The original branch contains ~2000 lines of changes that need to be split into manageable PRs while maintaining stability.

## Context: Building on Phase 10 Foundation

### Phase 10: FoundationDB Actor-based Peer Architecture (Available in Main)
The new transport layer PRs will build on top of Phase 10's actor-based Peer implementation, which is already completed and available in main branch. This established foundation provides:

**Key Foundation Components:**
- **TaskProvider trait**: Abstraction for spawning local tasks in single-threaded context
- **Actor-based Peer**: Single background task handling both read/write TCP operations  
- **Synchronous send() API**: Non-blocking message queuing (matches FoundationDB pattern)
- **Event-driven coordination**: Uses `Rc<Notify>` for wake-on-data notifications
- **Channel-based receive**: Background task forwards data via mpsc channels

**Architecture Pattern (From Phase 10):**
```rust
pub struct Peer<N, T, TP> {
    connection_handle: Option<JoinHandle<()>>,  // Single background actor
    send_queue: Rc<RefCell<VecDeque<Vec<u8>>>>, // Shared message queue
    receive_rx: mpsc::UnboundedReceiver<Vec<u8>>, // Incoming data channel
    data_to_send: Rc<Notify>,                   // Wake-on-send signal
    task_provider: TP,                          // Spawning abstraction
}
```

**Critical Phase 10 Discoveries:**
1. **RefCell + Async Conflict**: Cannot hold `RefCell` borrows across `.await` points
2. **FoundationDB Pattern Works**: Synchronous API + background actor eliminates borrow conflicts
3. **Event-driven Reading**: Uses `tokio::select!` for shutdown, send notifications, and continuous polling
4. **TaskProvider Abstraction**: Enables deterministic task spawning in simulation context

### Phase 11: Sans I/O Transport Layer (To Be Rebuilt Incrementally)
The transport layer work from pz/peers branch built a transport abstraction **on top of** the Phase 10 Peer foundation. This needs to be rebuilt incrementally with proper chaos testing:

**Relationship to Phase 10:**
- **Reuses Peer actors**: TransportDriver manages pool of Phase 10 Peer instances
- **Leverages TaskProvider**: All transport components use TaskProvider for spawning
- **Builds on event model**: Transport protocol generates events, Peer actors process I/O
- **Maintains single-threaded**: All components work within LocalSet constraints

**Added Layers (Phase 11):**
```
Application (Ping-Pong Actors)
    ↓
NetTransport Trait (get_reply, send_reply, bind)
    ↓  
ClientTransport / ServerTransport (request-response semantics)
    ↓
TransportDriver (peer pool management)
    ↓
TransportProtocol (Sans I/O state machine) 
    ↓
Phase 10 Peer Pool (background actors for actual TCP I/O)
```

This layered approach enables:
- **Protocol testing**: Sans I/O protocol can be tested without any network I/O
- **Request-response semantics**: get_reply() with correlation IDs built on top of raw Peer send/receive
- **Clean application API**: FlowTransport-style interface hiding transport complexity

## Current Branch Status

The `pz/peers` branch implements a complete FoundationDB-inspired transport layer but had to disable buggify/chaos testing to achieve stability. Key issues:
- 86/87 tests pass but only with chaos disabled
- Server transport deadlock bug (fixed in uncommitted changes)
- Requires shutdown coordination for test completion
- Complex interdependencies between components

## Architecture Overview

### Sans I/O Design Principles

**Core Philosophy:** Separate protocol logic from I/O operations for deterministic testing and better abstraction.

```
┌─────────────────────────────────────────────────────────────┐
│                    Application Layer                        │
│              (PingPong Actors, etc.)                       │
└─────────────────────┬───────────────────────────────────────┘
                      │
┌─────────────────────▼───────────────────────────────────────┐
│                  NetTransport Trait                        │
│          (get_reply, send_reply, bind, etc.)               │
└─────────────────────┬───────────────────────────────────────┘
                      │
        ┌─────────────▼──────────────┐
        │     ServerTransport        │     ┌─────────────────────┐
        │     ClientTransport        │────▶│  TransportDriver    │
        └────────────────────────────┘     └─────────┬───────────┘
                                                     │
                              ┌──────────────────────▼──────────────────────┐
                              │              TransportProtocol               │
                              │           (Sans I/O State Machine)          │
                              └──────────────────────┬──────────────────────┘
                                                     │
                              ┌──────────────────────▼──────────────────────┐
                              │                 Peer Pool                   │
                              │          (Actual Network I/O)              │
                              └─────────────────────────────────────────────┘
```

### Key Components

#### 1. Envelope Serialization Layer
**File:** `moonpool-simulation/src/network/transport/envelope.rs`

```rust
/// Trait for swappable envelope serialization strategies
pub trait EnvelopeSerializer: Clone {
    type Envelope: Debug + Clone;
    fn serialize(&self, envelope: &Self::Envelope) -> Vec<u8>;
    fn deserialize(&self, data: &[u8]) -> Result<Self::Envelope, SerializationError>;
}

/// Factory for creating envelopes with correlation IDs
pub trait EnvelopeFactory<S: EnvelopeSerializer> {
    fn create_request(correlation_id: u64, payload: Vec<u8>) -> S::Envelope;
    fn create_reply(request: &S::Envelope, payload: Vec<u8>) -> S::Envelope;
    fn extract_payload(envelope: &S::Envelope) -> &[u8];
}

/// Reply detection for request-response correlation
pub trait EnvelopeReplyDetection {
    fn is_reply_to(&self, correlation_id: u64) -> bool;
    fn correlation_id(&self) -> Option<u64>;
}
```

#### 2. Request-Response Envelope Implementation
**File:** `moonpool-simulation/src/network/transport/request_response_envelope.rs`

**Wire Format:** `[correlation_id:8][len:4][payload:N]`
- correlation_id: 8 bytes, little-endian u64
- len: 4 bytes, little-endian u32 (payload length)
- payload: N bytes of raw application data

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct RequestResponseEnvelope {
    pub correlation_id: u64,
    pub payload: Vec<u8>,
}

#[derive(Clone)]
pub struct RequestResponseSerializer;
```

Tests include serialization roundtrips, invalid data handling, and correlation ID matching.

#### 3. Sans I/O Protocol State Machine
**File:** `moonpool-simulation/src/network/transport/protocol.rs`

**Key Insight:** Protocol has NO I/O dependencies - everything passed as parameters.

```rust
pub struct TransportProtocol<S: EnvelopeSerializer> {
    serializer: S,
    transmit_queue: VecDeque<Transmit>,  // Outbound messages
    receive_queue: VecDeque<S::Envelope>, // Inbound processed messages
}

impl<S: EnvelopeSerializer> TransportProtocol<S> {
    // Pure state transitions
    pub fn send(&mut self, destination: String, envelope: S::Envelope);
    pub fn handle_received(&mut self, from: String, data: Vec<u8>);
    
    // Non-blocking polling for I/O driver
    pub fn poll_transmit(&mut self) -> Option<Transmit>;
    pub fn poll_receive(&mut self) -> Option<S::Envelope>;
    
    // Time passed as parameter (Sans I/O)
    pub fn handle_timeout(&mut self, now: Instant);
}
```

#### 4. Transport Driver (I/O Integration)
**File:** `moonpool-simulation/src/network/transport/driver.rs`

**Purpose:** Bridges Sans I/O protocol to actual Peer connections.

```rust
pub struct TransportDriver<N, T, TP, S> {
    protocol: TransportProtocol<S>,  // Pure state machine
    peers: HashMap<String, Rc<RefCell<Peer<N, T, TP>>>>, // Connection pool
    network: N,
    time: T,
    task_provider: TP,
}

impl<N, T, TP, S> TransportDriver<N, T, TP, S> {
    pub fn send(&mut self, destination: &str, envelope: S::Envelope) {
        // 1. Delegate to protocol (pure)
        self.protocol.send(destination.to_string(), envelope);
        // 2. Process transmissions (I/O)
        self.process_transmissions();
    }
    
    fn get_or_create_peer(&mut self, destination: &str) -> PeerHandle;
    pub fn process_peer_reads(&mut self); // Non-blocking
    pub async fn tick(&mut self); // Periodic maintenance
}
```

#### 5. NetTransport Trait (Application API)
**File:** `moonpool-simulation/src/network/transport/net_transport.rs`

**FlowTransport-equivalent API:**

```rust
#[async_trait(?Send)]
pub trait NetTransport<S: EnvelopeSerializer> {
    async fn bind(&mut self, address: &str) -> Result<(), TransportError>;
    
    // Actor-style request-response API with turbofish
    async fn get_reply<E>(&mut self, destination: &str, payload: Vec<u8>) 
        -> Result<Vec<u8>, TransportError>
    where
        E: EnvelopeFactory<S> + EnvelopeReplyDetection + 'static;
    
    fn send_reply(&mut self, request: &S::Envelope, payload: Vec<u8>) 
        -> Result<(), TransportError>;
    
    fn poll_receive(&mut self) -> Option<ReceivedEnvelope<S::Envelope>>;
    async fn tick(&mut self);
    async fn close(&mut self);
}
```

#### Turbofish Design Pattern for get_reply

The `get_reply<E>` method uses Rust's turbofish syntax (`::<Type>`) to explicitly specify the envelope type. This design choice enables several critical capabilities:

**Why Turbofish is Required:**

1. **Type Inference Limitations**: Rust cannot infer the envelope type `E` from context since it's only used in trait bounds, not in the return type.

2. **Multiple Envelope Support**: The transport layer is designed to be envelope-agnostic. Different envelope types can be used:
   ```rust
   // Request-Response envelopes (current implementation)
   transport.get_reply::<RequestResponseEnvelope>(dest, payload).await?;
   
   // Future: Rich Orleans-style envelopes
   transport.get_reply::<OrleansEnvelope>(dest, payload).await?;
   
   // Future: Custom user envelopes
   transport.get_reply::<CustomEnvelope>(dest, payload).await?;
   ```

3. **Trait Constraint Enforcement**: The where clause ensures the envelope type implements required traits:
   ```rust
   where E: EnvelopeFactory<S> + EnvelopeReplyDetection + 'static
   ```

**Implementation Details:**

```rust
// In ClientTransport::get_reply
async fn get_reply<E>(&mut self, destination: &str, payload: Vec<u8>) -> Result<Vec<u8>, TransportError>
where
    E: EnvelopeFactory<S> + EnvelopeReplyDetection + 'static,
{
    let correlation_id = self.next_correlation_id();
    let (tx, mut rx) = oneshot::channel();
    
    // Store response channel indexed by correlation ID
    self.pending_requests.insert(correlation_id, tx);
    
    // Use EnvelopeFactory to create typed envelope
    let envelope = E::create_request(correlation_id, payload);
    
    // Send through driver
    self.driver.send(destination, envelope);
    
    // Self-driving loop to process responses
    loop {
        self.poll_receive(); // Triggers correlation matching
        
        match rx.try_recv() {
            Ok(response_payload) => return Ok(response_payload),
            Err(TryRecvError::Empty) => {
                tokio::task::yield_now().await;
                continue;
            }
            Err(TryRecvError::Closed) => {
                return Err(TransportError::SendFailed("Request cancelled".to_string()));
            }
        }
    }
}
```

**Usage Pattern in Application Code:**

```rust
// Client ping implementation
async fn send_ping(&mut self) -> SimulationResult<()> {
    // Turbofish explicitly specifies RequestResponseEnvelope
    let response = self.transport
        .get_reply::<RequestResponseEnvelope>(&self.server_address, b"PING".to_vec())
        .await?;
    
    let message = String::from_utf8_lossy(&response);
    if message == "PONG" {
        Ok(())
    } else {
        Err(SimulationError::IoError(format!("Expected PONG, got: {}", message)))
    }
}
```

**Alternative Design Considered (Rejected):**

```rust
// REJECTED: Less flexible, couples transport to specific envelope type
async fn get_reply(&mut self, destination: &str, payload: Vec<u8>) 
    -> Result<Vec<u8>, TransportError>;
```

**Benefits of Turbofish Design:**

1. **Extensibility**: New envelope types can be added without changing the transport layer
2. **Type Safety**: Compile-time verification that envelope implements required traits  
3. **Clarity**: Explicit at call site which envelope type is being used
4. **Future-Proofing**: Supports Orleans-style rich metadata without API changes

**Correlation ID Flow with Turbofish:**

```
1. Client calls: transport.get_reply::<RequestResponseEnvelope>(dest, payload)
2. get_reply generates correlation_id = 42
3. EnvelopeFactory::create_request(42, payload) → RequestResponseEnvelope{correlation_id: 42, payload}
4. Envelope serialized and sent to server
5. Server processes and calls send_reply(request, response_payload)  
6. EnvelopeFactory::create_reply(request, response_payload) → RequestResponseEnvelope{correlation_id: 42, payload: response}
7. Response received by client transport
8. poll_receive() checks envelope.correlation_id() == 42
9. Matches pending request, sends response_payload through oneshot channel
10. get_reply loop receives response and returns to caller
```

This design enables clean request-response semantics while maintaining transport layer flexibility for future envelope evolution.

**Relationship to Phase 10 Architecture:**

The turbofish design was necessary because Phase 11 builds on Phase 10's raw byte-oriented Peer API:

```rust
// Phase 10 Peer API (raw bytes)
impl Peer<N, T, TP> {
    fn send(&mut self, data: Vec<u8>) -> Result<(), PeerError>;
    fn try_receive(&mut self) -> Option<Vec<u8>>;
}

// Phase 11 Transport API (typed envelopes)  
impl NetTransport<S> {
    async fn get_reply<E>(&mut self, dest: &str, payload: Vec<u8>) -> Result<Vec<u8>, TransportError>
    where E: EnvelopeFactory<S> + EnvelopeReplyDetection;
}
```

The transport layer needs to:
1. **Convert** typed envelopes → raw bytes (for Phase 10 Peer.send())
2. **Convert** raw bytes → typed envelopes (from Phase 10 Peer.try_receive())  
3. **Correlate** requests with responses using envelope metadata
4. **Support** multiple envelope formats without changing the underlying Peer infrastructure

The turbofish `<E>` parameter enables this type conversion while keeping the Peer layer envelope-agnostic.

#### 6. Client/Server Transport Implementations

**ServerTransport** (`moonpool-simulation/src/network/transport/server.rs`):
- Manages TCP listener and accepted connections
- Direct stream I/O for responses (not through peer system)
- **CRITICAL BUG:** Control flow issue in tick() method

**ClientTransport** (`moonpool-simulation/src/network/transport/client.rs`):
- Uses driver/peer system for connections
- Implements self-driving get_reply() to prevent deadlocks
- Correlation ID management for pending requests

## Critical Bugs and Fixes

### 1. Server Transport Control Flow Bug
**File:** `moonpool-simulation/src/network/transport/server.rs:179-239`

**Problem:** Server skips I/O operations after first tick, causing deadlock.

**Bug:** Write/read logic placed inside `if self.client_stream.is_none()` conditional:
```rust
// BUG: This only runs on first tick when client_stream is None
if self.client_stream.is_none() && let Some(ref listener) = self.listener {
    // Accept connection...
    
    // BUG: Write and read logic here - only runs once!
    if let Some((ref mut stream, ref peer_addr)) = self.client_stream {
        // Process pending writes...
        // Read from connection...
    }
}
```

**Fix:** Move write/read logic outside the conditional:
```rust
// Accept connection if needed
if self.client_stream.is_none() && let Some(ref listener) = self.listener {
    // Accept logic only
}

// FIXED: Write/read logic runs on every tick
if let Some((ref mut stream, ref peer_addr)) = self.client_stream {
    // Process pending writes...
    // Read from connection...
}
```

### 2. get_reply Deadlock
**File:** `moonpool-simulation/src/network/transport/client.rs:98-116`

**Problem:** get_reply() blocks awaiting response but doesn't drive transport.

**Fix:** Self-driving implementation:
```rust
async fn get_reply<E>(&mut self, destination: &str, payload: Vec<u8>) -> Result<Vec<u8>, TransportError> {
    let correlation_id = self.next_correlation_id();
    let (tx, mut rx) = oneshot::channel();
    
    self.pending_requests.insert(correlation_id, tx);
    let envelope = E::create_request(correlation_id, payload);
    self.driver.send(destination, envelope);
    
    // Self-driving: continuously poll transport while waiting
    loop {
        self.poll_receive(); // Drive message processing
        
        match rx.try_recv() {
            Ok(response_payload) => return Ok(response_payload),
            Err(TryRecvError::Empty) => {
                tokio::task::yield_now().await; // Yield and continue
                continue;
            }
            Err(TryRecvError::Closed) => return Err(TransportError::SendFailed("Cancelled".to_string())),
        }
    }
}
```

### 3. Shutdown Coordination
**File:** `moonpool-simulation/src/runner.rs:547-626`

**Problem:** Tests hang when client completes before server.

**Solution:** Runner sends shutdown signals:
```rust
// Create shutdown channels for each workload
let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
shutdown_senders.push(shutdown_tx);

// When first workload completes, signal all others
if !shutdown_triggered {
    for shutdown_sender in shutdown_senders.drain(..) {
        let _ = shutdown_sender.send(());
    }
    shutdown_triggered = true;
}
```

**Server implementation:**
```rust
async fn run(&mut self, mut shutdown_rx: tokio::sync::oneshot::Receiver<()>) -> SimulationResult<SimulationMetrics> {
    loop {
        select! {
            shutdown_result = &mut shutdown_rx => {
                self.transport.close().await;
                return Ok(SimulationMetrics::default());
            }
            _ = self.do_transport_work() => {
                // Continue processing
            }
        }
    }
}
```

## Current Test Structure

### Ping-Pong Actors
**File:** `moonpool-simulation/tests/simulation/ping_pong/actors.rs`

**Before:** 808 lines of complex TCP handling
**After:** 243 lines using transport abstraction

```rust
// New simplified server
pub struct Server<N, T, TP> {
    transport: ServerTransport<N, T, TP, RequestResponseSerializer>,
    bind_address: String,
}

// New simplified client  
pub struct Client<N, T, TP> {
    transport: ClientTransport<N, T, TP, RequestResponseSerializer>,
    server_address: String,
}

// Client ping implementation
async fn send_ping(&mut self) -> SimulationResult<()> {
    let response = self.transport
        .get_reply::<RequestResponseEnvelope>(&self.server_address, b"PING".to_vec())
        .await?;
    
    let message = String::from_utf8_lossy(&response);
    if message == "PONG" {
        Ok(())
    } else {
        Err(SimulationError::IoError(format!("Expected PONG, got: {}", message)))
    }
}
```

## PR Split Plan

### PR 1: Foundation Transport + Ping-Pong (Large but Necessary)
**Size:** ~1000 lines
**Rationale:** Core dependencies are too tightly coupled to split further

**Dependencies on Phase 10 Foundation (Already in Main):**
- **Uses existing TaskProvider**: All transport components use TaskProvider for spawning (already available)
- **Uses existing Peer actors**: TransportDriver manages pool of Phase 10 Peer instances (already available)
- **Uses Peer send/receive API**: Raw byte interface for actual network I/O (already available)
- **Leverages Peer event model**: Transport protocol events trigger Peer actor wake-ups (already available)

**Files:**
- `envelope.rs` - Basic trait and error types
- `request_response_envelope.rs` - Implementation with tests  
- `protocol.rs` - Sans I/O state machine with tests
- `driver.rs` - Basic driver with retry logic (uses Phase 10 Peer pool)
- `net_transport.rs` - Trait definition
- `server.rs` - ServerTransport with shutdown support
- `client.rs` - ClientTransport with basic get_reply
- `actors.rs` - Updated ping-pong actors with shutdown hooks
- `runner.rs` - Shutdown coordination support
- `types.rs` - Transmit struct

**Key Integration Points:**
- **TransportDriver creates Peer instances**: Uses Phase 10 `Peer::new(network, time, task_provider, destination, config)`
- **Driver uses Peer.send()**: Converts envelope bytes to Phase 10 raw byte send
- **Driver polls Peer.try_receive()**: Converts raw bytes back to envelopes
- **Shutdown coordination**: Uses same TaskProvider for transport tasks and Peer background actors

**Key Features:**
- Shutdown hooks (required or test hangs)
- Basic timeout/retry (required for chaos)
- Simple correlation IDs (required for request/response)
- Server control flow fix (required to prevent deadlock)

**Tests (Must Pass with Chaos Enabled):**
```rust
#[test]
fn test_ping_pong_with_chaos() {
    SimulationBuilder::new()
        .set_iteration_control(IterationControl::UntilAllSometimesReached(100))
        .enable_buggify() // ENABLED from start
        .register_workload("server", ping_pong_server)
        .register_workload("client", ping_pong_client)
        .run()
        .await;
}

#[test] 
fn test_envelope_survives_corruption() {
    // Test envelope serialization with corrupted data
}

#[test]
fn test_protocol_retries_on_failure() {
    // Test Sans I/O protocol handles failures
}

#[test]
fn test_server_control_flow_fix() {
    // Verify server processes I/O on every tick
}

#[test]
fn test_shutdown_coordination() {
    // Verify clean shutdown with multiple workloads
}
```

### PR 2: Add Real Tokio Test
**Size:** ~200 lines
**Goal:** Validate transport works outside simulation

**New File:**
- `tests/simulation/ping_pong/tokio.rs`

**Implementation:**
```rust
#[test]
fn test_ping_pong_with_real_tokio() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    
    runtime.block_on(async {
        // Use TokioNetworkProvider (real TCP)
        // Use TokioTimeProvider (real time)
        // Run same ping-pong actors
        // Verify it works without simulation
    });
}

#[test]
fn test_transport_with_real_network() {
    // Test transport with actual TCP connections
}

#[test]
fn test_timeouts_with_real_time() {
    // Test timeout behavior with real time passage
}
```

**Critical Validation:**
- Ensures we're not accidentally depending on simulation-specific behavior
- Catches timing assumptions
- Validates real-world usage

### PR 3: Make get_reply Self-Driving
**Size:** ~100 lines
**Goal:** Fix deadlock in request/response pattern

**Changes:**
- Convert get_reply from returning Future to async fn
- Make it poll transport while waiting
- Add proper timeout handling

**Tests:**
```rust
#[test]
fn test_get_reply_no_deadlock() {
    // Verify get_reply doesn't deadlock waiting for response
}

#[test]
fn test_concurrent_get_reply() {
    // Multiple concurrent get_reply calls
}

#[test]
fn test_get_reply_with_tokio() {
    // Test with real Tokio runtime
}
```

### PR 4: Improve Chaos Resilience
**Size:** ~300 lines
**Goal:** Handle more chaos scenarios

**Improvements:**
- Better connection pool management
- Exponential backoff with jitter
- Circuit breaker for failing peers
- Dead letter queue for undeliverable messages

**Tests:**
```rust
#[test]
fn test_connection_pool_recovery() {
    // Test peer connection recovery after failures
}

#[test]
fn test_circuit_breaker_triggers() {
    // Test circuit breaker pattern
}

#[test]
fn test_exponential_backoff() {
    // Test retry behavior under failures
}
```

### PR 5: Add Peer Management to Driver
**Size:** ~200 lines
**Goal:** Automatic peer lifecycle management

**Changes:**
- Driver creates peers on demand
- Cleanup idle connections
- Connection health monitoring

**Tests:**
```rust
#[test]
fn test_peer_lifecycle() {
    // Test automatic peer creation/cleanup
}

#[test]
fn test_idle_connection_cleanup() {
    // Test cleanup of unused connections
}
```

### PR 6: Multi-Server Support
**Size:** ~300 lines
**Goal:** Test with multiple servers

**New Test:**
- `tests/simulation/ping_pong/multi_server.rs`
- Client talks to multiple servers
- Load balancing and failover

**Tests:**
```rust
#[test]
fn test_multi_server_ping_pong() {
    // Client communicates with multiple servers
}

#[test]
fn test_server_failover() {
    // Client handles server failures
}
```

## Implementation Guidelines

### Testing Strategy: Three-Layer Approach

1. **Unit Tests:** Test components in isolation
   - Envelope serialization
   - Protocol state machine
   - Driver peer management

2. **Simulation Tests:** Test with chaos and determinism  
   - Always run with buggify enabled
   - 100+ iterations for robustness
   - Deterministic failure reproduction

3. **Tokio Tests:** Test with real runtime and network
   - Catch simulation assumptions
   - Validate real-world behavior
   - Performance characteristics

### Chaos-First Development Principles

1. **Never Disable Chaos:** Every PR must pass with buggify enabled
2. **Timeouts Everywhere:** Every async operation has timeout
3. **Retry by Default:** Assume failures and retry appropriately
4. **Graceful Degradation:** Handle partial failures cleanly
5. **Observable Failures:** Log and metrics for debugging

### Key Success Metrics

- All tests pass with chaos enabled (no exceptions)
- Get_reply never deadlocks under any conditions
- Server handles connection failures gracefully
- Clean shutdown under all failure scenarios
- Performance acceptable under light chaos (not just heavy)

## Lessons Learned

1. **Shutdown Coordination is Essential:** Without it, tests hang unpredictably
2. **Control Flow Bugs are Subtle:** Server I/O bug was hard to diagnose
3. **get_reply Must Be Self-Driving:** Any blocking wait on transport needs driving
4. **Chaos Testing Cannot Be Deferred:** Must be enabled from the start
5. **Real Tokio Testing Catches Edge Cases:** Simulation can hide timing issues
6. **Tight Coupling Makes Small PRs Hard:** Some components cannot be meaningfully separated

## Dependencies Between PRs

- **PR 1 → All Others:** Foundation required
- **PR 2 → PR 3+:** Tokio test validates each improvement
- **PR 3 → PR 4+:** Self-driving get_reply enables higher chaos
- **PR 4 → PR 5+:** Resilience enables more complex scenarios
- **PR 5 → PR 6:** Peer management enables multi-server

## Summary

This plan acknowledges that some complexity cannot be avoided - the transport layer has fundamental interdependencies that make very small PRs impossible. However, by building chaos resilience from the start and validating with real Tokio tests early, we can ensure each increment is solid and builds proper foundation for the next.

The key insight is to front-load the essential complexity (PR 1) while maintaining rigorous testing standards, then iterate on improvements with smaller, focused PRs.