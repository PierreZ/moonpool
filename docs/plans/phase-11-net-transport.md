# Phase 11: Sans I/O Network Transport Layer

## Overview

Phase 11 implements a Sans I/O network transport layer that provides request-response semantics on top of raw TCP connections. This phase represents a significant architectural improvement that enables building complex protocols with proper separation of concerns.

### Key Design Principles

1. **Sans I/O Pattern**: Protocol logic separated from actual I/O operations
2. **Envelope-based Messaging**: All communication through typed envelopes
3. **Correlation IDs**: Request-response tracking for reliable communication
4. **Chaos Testing**: Built-in support for fault injection from the start
5. **Provider Pattern**: Abstract dependencies for testability

## Completed Phases

### Phase 11.0: Core Envelope Traits (✅ COMPLETED - 161 lines)
- Core envelope traits and marker types
- Test infrastructure setup
- Foundation for entire transport layer

### Phase 11.1: Serialization Layer (✅ COMPLETED - 120 lines)
- Binary wire format for envelopes
- Efficient serialization/deserialization
- Length-prefixed messages

### Phase 11.2: Sans I/O Protocol State Machine (✅ COMPLETED - 336 lines)
- Pure state machine for protocol logic
- No actual I/O operations
- Comprehensive testing of protocol states

### Phase 11.3: Protocol-to-Peer Driver (✅ COMPLETED - 175 lines)
- Bridges Sans I/O protocol with actual networking
- Manages buffers and I/O operations
- Self-driving futures

### Phase 11.4: Client Transport (✅ COMPLETED - 338 lines)
- High-level client API with turbofish pattern
- Self-driving get_reply method
- Automatic reconnection on failures

### Phase 11.5: Server Transport (✅ COMPLETED - 283 lines)
- Connection acceptance and management
- Request handling with proper replies
- End-to-end ping-pong test

### Phase 11.6: Chaos Testing (✅ COMPLETED - 131 lines)
- Deterministic fault injection
- Connection failures and recovery
- Network chaos scenarios
- Fixed connection cut resilience

## Transport Architecture

### Current Architecture: Sans I/O + Driver Pattern

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Client    │────▶│   Driver    │────▶│   Protocol  │
│  Transport  │     │  (I/O ops)  │     │ (Sans I/O)  │
└─────────────┘     └─────────────┘     └─────────────┘
                           │
                           ▼
                    ┌─────────────┐
                    │    Peer     │
                    │  (TcpStream)│
                    └─────────────┘
```

### Key Components

1. **Envelope System**: Type-safe message containers with correlation
2. **Protocol State Machine**: Pure logic, no I/O
3. **Driver Layer**: Bridges protocol to actual networking
4. **Transport Layer**: High-level API for clients and servers

## Lessons Learned

### What Worked Well

1. **Sans I/O Pattern**: Clean separation enables thorough testing
2. **Incremental Phases**: Each phase tested independently
3. **Early Chaos Testing**: Found critical bugs early
4. **Self-Driving Futures**: Simplified API usage
5. **Provider Pattern**: Easy mocking and simulation

### Challenges Encountered

1. **Transport Coordination is Essential:** Without proper coordination, tests hang unpredictably
2. **Control Flow Bugs are Subtle:** Server I/O bug was hard to diagnose
3. **get_reply Must Be Self-Driving:** Any blocking wait on transport needs driving
4. **Chaos Testing Cannot Be Deferred:** Must be enabled from the start
5. **Tight Coupling Makes Small PRs Hard:** Some components cannot be meaningfully separated

## Phase Benefits & Risk Mitigation

### Benefits of 7-Phase Approach

1. **Reviewable Size:** Each phase 150-450 lines vs 1000+ line monolith
2. **Early Validation:** Can test envelope/protocol logic before any networking
3. **Clear Milestones:** Each phase has specific, measurable goals
4. **Rollback Friendly:** Easy to revert individual phases without losing everything
5. **Incremental Chaos:** Can introduce chaos testing gradually
6. **Reduced Risk:** Problems found early, before complex integration
7. **Better Reviews:** Smaller chunks allow thorough review

### Risk Mitigation Strategies

**Integration Risks:**
- Phase 11.3 validates driver-to-peer integration early
- Phase 11.5 completes end-to-end testing before chaos
- Each phase includes integration tests where applicable

**Chaos Testing Risks:**
- Deterministic testing through Phase 11.5
- Gradual chaos introduction in Phase 11.6
- All previous phases must work before enabling chaos

**API Design Risks:**
- Turbofish pattern validated in Phase 11.4
- ClientTransport API established before server implementation
- Self-driving get_reply tested before chaos scenarios

**Performance Risks:**
- Basic performance tested in Phase 11.5
- Chaos performance validated in Phase 11.6
- Optimizations deferred to optional Phase 11.7

## Dependencies Between Phases

- **Phase 10 → 11.3:** Driver requires existing Peer infrastructure
- **11.0 → 11.1 → 11.2:** Sequential envelope → serialization → protocol
- **11.2 → 11.3:** Driver wraps protocol
- **11.3 → 11.4:** Client uses driver for peer management
- **11.4 → 11.5:** Server complements client for end-to-end
- **11.5 → 11.6:** Complete system required before chaos testing
- **11.6 → 11.7:** Chaos resilience required for advanced features

## Summary

This 7-phase approach transforms Phase 11 from a risky 1000+ line implementation into a series of manageable, well-tested increments. Each phase builds systematically on the previous, with clear success criteria and early validation opportunities.

**Key Advantages:**
- **No giant PRs:** Largest phase is ~450 lines
- **Testable at every step:** Each phase validates specific functionality
- **Incremental complexity:** Start simple, add features gradually
- **Chaos-ready:** System designed for chaos from the ground up
- **Review-friendly:** Small chunks enable thorough code review
- **Risk mitigation:** Problems caught early, before integration complexity

The approach acknowledges that transport layer complexity is inherent, but distributes it across phases to make each step manageable and verifiable.

---

## URGENT: Fix Connection Cut Resilience Issue

### Problem Description

With random config enabled, the server transport incorrectly treats temporarily cut connections as permanent failures, causing deadlocks. The issue manifests as:

```
2025-09-21T16:30:45.741981Z DEBUG run:step: moonpool_simulation::sim: Randomly cut connection 1 until 5.339934928s
2025-09-21T16:30:45.742227Z  WARN run: moonpool_simulation::network::transport::server: Server: Write failed: Connection was cut
2025-09-21T16:30:45.742252Z DEBUG run: moonpool_simulation::network::sim::stream: SimTcpStream dropping, closing connection 1
2025-09-21T16:30:45.742265Z DEBUG run: moonpool_simulation::sim: Connection 1 closed permanently
```

### Root Cause Analysis

**Issue 1: Server Transport Connection Cut Handling**
Location: `moonpool-simulation/src/network/transport/server.rs:218-223`

The server transport incorrectly treats `ConnectionReset` errors (from cut connections) as permanent failures:

```rust
// PROBLEM: Treats temporary cuts as permanent failures
match stream.write_all(&data).await {
    Ok(_) => { /* success */ }
    Err(e) => {
        tracing::warn!("Server: Write failed: {}", e);
        // BUG: Marks connection as failed permanently
        connection_failed = true;
        break;
    }
}
```

When a connection is cut, `poll_write` returns `ConnectionReset` error, but the server immediately:
1. Marks connection as failed (`connection_failed = true`)
2. Closes the connection permanently (`self.client_stream = None`)
3. Never attempts to reconnect or retry

**Issue 2: Stream Drop on Cut**
Location: `moonpool-simulation/src/network/sim/stream.rs:402-413`

When a connection is cut, the SimTcpStream gets dropped incorrectly:
```rust
impl Drop for SimTcpStream {
    fn drop(&mut self) {
        // BUG: This runs when connection is cut, not just on real close
        tracing::debug!("SimTcpStream dropping, closing connection {}", self.conn_id);
        if let Ok(sim) = self.sim.lock() {
            if let Some(conn) = sim.connections.get_mut(&self.conn_id) {
                conn.closed = true; // Marks as permanently closed!
            }
        }
    }
}
```

### Solution Design

**Principle: Temporary cuts should not close connections permanently**

Following FoundationDB's resilience pattern, the transport should:
1. **Detect cut vs closed**: Differentiate between temporary cuts and permanent closure
2. **Queue during cuts**: Buffer writes during cut periods
3. **Resume on restore**: Continue operations when cut is lifted
4. **Only close on real disconnect**: Client explicitly closing or fatal errors

**Implementation Fix:**

1. **Server Transport Fix** - Buffer writes during cuts:
```rust
// Don't mark connection as failed on ConnectionReset
match stream.write_all(&data).await {
    Ok(_) => {
        tracing::trace!("Server: Sent {} bytes", data.len());
    }
    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
        // Connection cut - put data back and continue
        tracing::debug!("Server: Connection cut, buffering write");
        self.pending_writes.push_front(data);
        // Don't mark as failed, just break inner loop
        break;
    }
    Err(e) => {
        // Real error - connection failed
        tracing::warn!("Server: Write failed: {}", e);
        connection_failed = true;
        break;
    }
}
```

2. **Connection Restoration**: When the cut connection is restored:
   - Simulation restores the connection and wakes read wakers
   - Server retries pending writes on next tick()
   - Server can successfully read incoming data
   - Communication resumes normally

3. **Test Success**: With this fix:
   - Random connection cuts will be handled gracefully
   - Ping-pong exchanges will complete despite temporary cuts
   - Server will only close connections on genuine client disconnection
   - Tests will pass with chaos/random config enabled

### Implementation Priority

This is a **critical fix** required for Phase 11.6 completion. Without this fix:
- Tests fail with random config enabled
- Transport layer appears unreliable under chaos conditions
- Cannot proceed to advanced chaos testing scenarios

The fix should be implemented immediately to enable proper chaos testing of the transport layer.

---

## Phase 11.7: Developer Experience Improvements - FoundationDB-Inspired Patterns

### Overview

Based on analysis of FoundationDB's Flow implementation and the current transport layer complexity, this phase focuses on vastly improving developer experience by adopting FoundationDB-inspired patterns for handling concurrent events.

### Current Problems

The current ping-pong actors suffer from several developer experience issues:

1. **Manual tick() calls** - Developers must manually drive the transport layer
2. **Complex shutdown handling** - Multiple shutdown paths with different mechanisms  
3. **Manual state tracking** - Developers manually track messages_handled, consecutive_failures
4. **Nested conditionals** - Complex if/else chains that are hard to follow
5. **Explicit yield management** - Manual yield_now() calls scattered throughout
6. **Multiple exit points** - Return statements scattered throughout the loop
7. **MAX_PING artificial limits** - Server has unnecessary message counting

### FoundationDB-Inspired Solution

#### 1. **Stream-based Message Reception (like RequestStream)**
FoundationDB uses `waitNext()` on RequestStreams in choose/when blocks. We should provide a similar async stream API.

**New Transport API:**
```rust
impl ServerTransport {
    /// Returns a stream that yields Result for proper error handling
    pub fn message_stream(&self) -> impl Stream<Item = Result<ReceivedEnvelope, TransportError>> {
        // Yields Ok(envelope) for messages
        // Yields Err(e) for connection errors, parse errors, etc.
        // Stream continues after recoverable errors
    }
}

impl ClientTransport {
    /// Simple request-response - fully async, no manual loops
    pub async fn request(&mut self, addr: &str, payload: Vec<u8>) -> Result<Vec<u8>, TransportError> {
        // Self-driving, no manual tick() or poll_receive() calls
    }
}
```

#### 2. **Single tokio::select! Pattern**
Replace entire complex loops with a single tokio::select! that handles all events, following FoundationDB's choose/when pattern.

**Simplified Server (remove MAX_PING entirely):**
```rust
pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
    self.transport.bind(&self.topology.my_ip).await?;
    let mut messages = self.transport.message_stream();
    
    loop {
        tokio::select! {
            // Handle incoming messages OR transport errors
            Some(result) = messages.next() => {
                match result {
                    Ok(msg) if msg.payload() == b"PING" => {
                        self.transport.send_reply(&msg, b"PONG").await?;
                        // No counting, no MAX_PING check
                    }
                    Ok(_) => {
                        // Ignore non-PING messages
                    }
                    Err(e) => {
                        // Transport error - connection lost, read failed, etc.
                        tracing::warn!("Transport error: {}", e);
                        // Could reconnect or just continue for next connection
                    }
                }
            }
            
            // Clean shutdown
            _ = self.topology.shutdown_signal.cancelled() => {
                self.transport.close().await;
                return Ok(SimulationMetrics::default());
            }
        }
    }
}
```

**Simplified Client (keep MAX_PING for controlled termination):**
```rust
pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
    while self.messages_sent < MAX_PING {
        tokio::select! {
            result = self.transport.request(&self.server_address, b"PING") => {
                match result {
                    Ok(response) if response == b"PONG" => {
                        self.messages_sent += 1;
                    }
                    Err(TransportError::ConnectionLost) => {
                        // Retry with backoff
                        continue;
                    }
                    Err(e) => {
                        return Err(SimulationError::IoError(format!("Fatal: {}", e)));
                    }
                    _ => {
                        return Err(SimulationError::IoError("Invalid response"));
                    }
                }
            }
            
            _ = self.topology.shutdown_signal.cancelled() => {
                return Ok(SimulationMetrics::default());
            }
        }
    }
    
    Ok(SimulationMetrics::default())
}
```

#### 3. **Self-Driving Transport with Internal Actors**
Following FoundationDB's connectionReader/connectionWriter pattern, the transport should spawn internal actors to handle all I/O automatically.

**No Manual Driving Required:**
- Transport spawns background actors on bind/connect
- These actors run independently like FoundationDB's connection actors
- Developers never call tick() manually
- All I/O happens transparently

#### 4. **Error Handling (FDB-style Result Propagation)**
Following FoundationDB's philosophy where errors propagate through futures/streams:

**Stream Returns Result:**
```rust
// Transport errors are explicit and must be handled
Some(result) = messages.next() => {
    match result {
        Ok(msg) => { /* handle message */ }
        Err(TransportError::ConnectionLost) => { /* handle recoverable error */ }
        Err(e) => { /* handle fatal error */ }
    }
}
```

**Error Types to Handle:**
- `ConnectionLost` - Client disconnected (recoverable)
- `ReadError` - I/O failure (may be recoverable)
- `ParseError` - Invalid message format (protocol error)
- `Timeout` - Operation timed out
- `BindFailed` - Server bind error (fatal)

### Benefits of This Approach

1. **90% Code Reduction**: Server actor goes from 130+ lines to ~20 lines
2. **Clear Control Flow**: Single tokio::select! handles all events
3. **Type-Safe Error Handling**: Compiler ensures all errors are handled
4. **FoundationDB-Proven Pattern**: Based on production-tested distributed systems code
5. **No Manual Transport Driving**: Transport manages itself with internal actors
6. **Testable**: Easy to mock transport streams for unit tests
7. **Maintainable**: Clear separation between transport and application logic

### Incremental Implementation Plan

**CRITICAL**: The original "big bang" approach causes deadlocks when mixing manual tick() with stream consumption. This incremental plan avoids that issue.

#### Step 1: Enhanced Error Types (✓ COMPLETED)
- Add ConnectionLost, ReadError, ParseError to TransportError enum
- Foundation for better error handling throughout

#### Step 2: Simple API Rename
- Rename `get_reply()` to `request()` in ClientTransport
- Update all test references
- **Safe, non-breaking change**
- Test with tokio runner to verify

#### Step 3: Simplify Actors WITHOUT Stream API
- Keep using tick() and poll_receive() 
- Restructure code for clarity:
  - Single main loop
  - Reduced nesting
  - Centralized shutdown handling
- Verify tests still pass

#### Step 4: Add Parallel Stream API (Non-Breaking)
- Add `message_stream()` as ALTERNATIVE to poll_receive()
- Keep existing tick()/poll_receive() working
- Stream uses internal buffer fed by tick()
- Both APIs work independently

#### Step 5: Migrate Server to Hybrid Approach
- Server uses message_stream() 
- Still calls tick() manually to drive I/O
- Stream reads from buffer filled by tick()
- Test with existing client

#### Step 6: Simplify Client Pattern
- Use renamed request() method
- Simplify loop structure
- Remove unnecessary error tracking

#### Step 7: Self-Driving Transport (Final Goal)
- Only after everything else works
- Spawn internal actors in bind()/connect()
- Remove manual tick() calls
- Full FoundationDB-style automation

### Files to Modify

1. **Server Transport** (`moonpool-simulation/src/network/transport/server.rs`):
   - Add `message_stream()` method returning `impl Stream<Item = Result<ReceivedEnvelope, TransportError>>`
   - Spawn internal actors for I/O management
   - Remove manual tick() requirement

2. **Client Transport** (`moonpool-simulation/src/network/transport/client.rs`):
   - Add simple `request()` method
   - Make fully self-driving (no manual loops)
   - Internal actors handle all I/O

3. **Ping-Pong Actors** (`moonpool-simulation/tests/simulation/ping_pong/actors.rs`):
   - Remove MAX_PING logic from server
   - Convert both actors to single tokio::select! pattern
   - Remove all manual tick() calls
   - Implement proper error handling with Result types

This approach follows FoundationDB's core philosophy: hide transport complexity behind clean async APIs, use select/choose patterns for concurrent event handling, and let errors propagate naturally through the type system.

---

## Phase 11.8: Event-Driven Multi-Connection ServerTransport

### Problem Statement

The current `ServerTransport` implementation has fundamental limitations:
- **Single connection only** - Can handle only one client at a time
- **tick() based polling** - Requires manual driving instead of event-driven
- **Sequential processing** - Cannot handle concurrent requests
- **Poor developer experience** - Complex tick/poll pattern

### Analysis: How Distributed Systems Handle This

#### FoundationDB's Approach
- Uses `peers` HashMap to manage multiple connections
- Each peer has its own `connectionReader`/`connectionWriter` actors  
- Token-based routing via `EndpointMap`
- Messages from ANY connection with the same token go to the same handler
- No central message queue - direct delivery to endpoints

#### Orleans' MessageCenter Pattern
- `MessageCenter` orchestrates all message routing
- `Gateway` handles client connections
- `ConnectionManager` manages silo-to-silo connections
- Single `ReceiveMessage()` entry point that routes to appropriate handlers

### Design Decision: Single Queue Architecture

After analyzing both patterns, we choose a **single queue** design for `ServerTransport`:

**Rationale:**
1. **ServerTransport is low-level transport** - Should not understand routing
2. **MessageCenter will live in moonpool** - Higher-level routing logic belongs there
3. **Simplicity** - Single queue is easier to reason about
4. **Flexibility** - MessageCenter can implement any routing pattern on top

### Architecture Design

#### Core Structure
```rust
pub struct ServerTransport<N, T, TP, S> {
    // Providers
    network: N,
    time: T,
    task_provider: TP,
    serializer: S,
    
    // Single queue for ALL messages from ALL connections
    incoming_rx: mpsc::UnboundedReceiver<IncomingMessage>,
    
    // Connection management (Rc/RefCell for single-threaded)
    connections: Rc<RefCell<HashMap<ConnectionId, ConnectionHandle>>>,
}

pub struct IncomingMessage {
    pub envelope: S::Envelope,
    pub connection_id: ConnectionId,  // Source tracking
    pub peer_address: String,
}
```

#### Key Design Choices

1. **Single-threaded optimization**
   - Use `Rc<RefCell<>>` instead of `Arc<RwLock<>>`
   - No synchronization overhead
   - Simpler ownership model

2. **One actor per connection**
   - Each connection owns its `TcpStream`
   - Handles both read and write
   - Simplified from FDB's 3-actor model due to Rust ownership

3. **Event-driven with async/await**
   - No tick() method
   - `next_message()` blocks until message arrives
   - Pure async/await throughout

4. **Connection tracking**
   - Each message includes `connection_id`
   - Enables proper reply routing
   - MessageCenter can track per-connection state if needed

### Implementation Plan

#### Step 1: Create New Event-Driven ServerTransport
- Complete rewrite in new file
- Implement connection actor pattern
- Single queue for all messages
- No tick() or polling

#### Step 2: Update PingPongServerActor
- Remove tick() calls
- Use clean async/await pattern
- Single tokio::select! for all events

#### Step 3: Test Multi-Connection Scenarios
- Multiple clients connecting simultaneously
- Connection failures and recovery
- Concurrent request handling

#### Step 4: Remove Old Implementation
- Delete tick-based server.rs
- Update all references
- Clean up transport tests

### Benefits

1. **True event-driven** - No polling or manual driving
2. **Multi-connection support** - Handle many clients concurrently
3. **Clean abstraction** - Transport doesn't know about actors or routing
4. **Performance** - Single-threaded with no synchronization overhead
5. **Extensible** - Easy to build MessageCenter on top
6. **Developer friendly** - Simple async/await API

### API Example

```rust
// Server setup
let mut server = ServerTransport::bind(network, time, task_provider, serializer, "127.0.0.1:8080").await?;

// Simple event loop
loop {
    tokio::select! {
        // Receives from ANY connection
        Some(msg) = server.next_message() => {
            if msg.envelope.payload() == b"PING" {
                // Reply goes back to the same connection
                server.send_reply(&msg.envelope, b"PONG", &msg)?;
            }
        }
        
        _ = shutdown.cancelled() => break,
    }
}
```

This design provides a clean, efficient foundation for building distributed systems while keeping the transport layer simple and focused.