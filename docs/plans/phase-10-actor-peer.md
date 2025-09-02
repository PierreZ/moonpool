# Phase 10: Actor-based Peer Architecture

## Overview
Transform the Peer implementation from synchronous request-response to an actor-based architecture matching FoundationDB's pattern, adapted for single-threaded spawn_local context.

## Goals
- Implement three-actor pattern: ConnectionKeeper, ConnectionReader, ConnectionWriter
- Enable continuous background I/O independent of API calls
- Maintain deterministic single-threaded execution for simulation
- Improve fault tolerance through independent actor lifecycle management

## Phase 0: TaskProvider Trait
Create a TaskProvider trait to abstract task spawning, following the same pattern as NetworkProvider and TimeProvider.

```rust
/// Provider for spawning local tasks in single-threaded context
#[async_trait(?Send)]
pub trait TaskProvider {
    /// Spawn a named task that runs on the current thread
    fn spawn_task<F>(&self, name: &str, future: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + 'static;
}

/// Tokio-based task provider using spawn_local
#[derive(Clone, Debug)]
pub struct TokioTaskProvider;

#[async_trait(?Send)]
impl TaskProvider for TokioTaskProvider {
    fn spawn_task<F>(&self, name: &str, future: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + 'static,
    {
        let name = name.to_string();
        tokio::task::Builder::new()
            .name(&name)
            .spawn_local(async move {
                tracing::trace!("Task {} starting", name);
                future.await;
                tracing::trace!("Task {} completed", name);
            })
            .expect("Failed to spawn task")
    }
}
```

## Phase 1: Core Actor Structure
```rust
pub struct Peer<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    // Actor handles (LocalSet compatible)
    keeper_handle: Option<JoinHandle<()>>,
    reader_handle: Option<JoinHandle<()>>,
    writer_handle: Option<JoinHandle<()>>,
    
    // Shared state (no Arc needed - single threaded)
    send_queue: Rc<RefCell<VecDeque<Vec<u8>>>>,
    connection: Rc<RefCell<Option<N::TcpStream>>>,
    
    // Coordination (single-threaded versions)
    data_to_send: Rc<Notify>,
    shutdown: CancellationToken,
    
    config: PeerConfig,
    metrics: Rc<RefCell<PeerMetrics>>,
    
    // Provider instances
    task_provider: TP,
}
```

## Phase 2: Actor Implementation
### ConnectionKeeper Actor
- Main coordinator that manages connection lifecycle
- Implements exponential backoff logic
- Spawns reader/writer actors on successful connection
- Uses `select!` to restart actors on completion/failure

### ConnectionReader Actor  
- Continuous background reading from TCP stream
- Updates shared connection state on read failures
- Forwards received data to application via channel or buffer

### ConnectionWriter Actor
- Waits on `data_to_send.notified()` for new messages
- Drains send queue in batches
- Handles write failures by clearing shared connection

## Phase 3: API Transformation
### Peer::send() - Non-blocking
- Direct push to shared send queue
- Call `data_to_send.notify()` to wake writer
- No await - returns immediately

### Peer::receive() - Polling
- Poll shared receive buffer populated by reader actor
- Or use channel receiver for incoming data

### Lifecycle Management
- **Peer::new()**: Spawns keeper task immediately using `task_provider.spawn_task()`
- **Peer::close()**: Sends shutdown signal, awaits all JoinHandles
- **Drop implementation**: Ensures clean actor termination

## Phase 4: Integration & Testing
- Update existing tests to work with new async-only API
- Add actor coordination tests for startup/shutdown behavior
- Verify deterministic execution in simulation context
- Maintain compatibility with existing metrics collection

## Key Benefits
- **True concurrency**: Background I/O independent of API calls
- **Better fault tolerance**: Each actor can fail/restart independently  
- **FoundationDB compatibility**: Matches reference architecture patterns
- **Simulation optimized**: Single-threaded deterministic execution
- **Improved performance**: Continuous background processing vs on-demand

## Files to Modify
- `src/task/mod.rs`: New TaskProvider trait definition
- `src/task/tokio.rs`: New TokioTaskProvider implementation  
- `src/sim_world.rs`: Add task_provider() method
- `peer/core.rs`: Refactor Peer struct for actor coordination
- `peer/actors.rs`: New file with keeper/reader/writer actor functions
- Keep existing: `config.rs`, `metrics.rs`, `error.rs` unchanged

## Scope Limitations
- **Reliable delivery only**: All messages use reliable queue (no unreliable packets)
- **No protocol handshake**: Direct TCP connection without negotiation
- **Simulation focused**: Optimized for deterministic testing, not production networking

## Implementation Challenges Discovered

### RefCell Borrow + Async I/O Conflict
**Problem**: Cannot borrow `RefCell<SharedState>` across `.await` points in TCP I/O operations
- `stream.read(&mut buffer).await` requires holding mutable borrow
- RefCell borrows must be dropped before await points
- Creates deadlocks: actors spawn but can't perform I/O

**Root Cause**: Single-threaded simulation constraint conflicts with async TCP patterns
- `Rc<RefCell<T>>` for shared state (no Send/Sync)
- Async TCP I/O requires borrowing across await boundaries
- Simulation deadlock detection triggers when tasks can't progress

### Alternative Approaches
1. **Message passing**: Use channels instead of shared RefCell state
2. **Synchronous I/O**: Use blocking I/O with simulation time integration
3. **Future-based**: Store futures instead of direct I/O operations
4. **Incremental**: Keep existing Peer logic, add TaskProvider parameter only

**Recommendation**: Start with approach #4 - minimal change to working system

## Phase 0 Implementation Status: ✅ COMPLETED

### TaskProvider Integration Complete
- ✅ `TaskProvider` trait created in `src/task/mod.rs`
- ✅ `TokioTaskProvider` implemented in `src/task/tokio_provider.rs` 
- ✅ `SimWorld::task_provider()` method added
- ✅ `Peer<N, T, TP>` updated to include TaskProvider parameter
- ✅ All constructors updated: `Peer::new()`, `Peer::new_with_defaults()`
- ✅ All tests updated to pass TaskProvider parameter
- ✅ WorkloadFn signature updated to include TaskProvider
- ✅ All ping-pong actors updated for TaskProvider compatibility

## Phase 10 Implementation Status: ✅ COMPLETED

### FoundationDB Synchronous API Pattern Implemented
- ✅ **Synchronous send() API**: Returns immediately after queuing (no async I/O blocking)
- ✅ **Background connection task**: Single actor handles both read/write TCP operations using event-driven model
- ✅ **Trigger-based coordination**: Uses `Rc<Notify>` to wake task when data queued
- ✅ **RefCell conflict resolution**: No RefCell borrows held across await points
- ✅ **Channel-based receive**: Background task sends data via mpsc::unbounded_channel
- ✅ **API compatibility**: receive() returns Vec<u8> instead of filling buffer
- ✅ **Function naming**: Renamed connection_writer_actor → connection_task (handles both R/W)
- ✅ **Early exit on deadlock**: Simulation runner exits immediately on deadlock detection
- ✅ **Full event-driven model**: Connection task uses `tokio::select!` for:
  - Shutdown signal monitoring (`shutdown_rx.recv()`)
  - Data-to-send notifications (`data_to_send.notified()`)
  - Continuous reading with polling (`async { time.sleep(1ms) }`)
- ✅ **Testing strategy**: 
  - Default: `UntilAllSometimesReached(1000)` for comprehensive chaos testing
  - Debug: `FixedCount(1)` with ERROR level filtering for faulty seed investigation

### Architecture Deviation from Original Plan
**Used FoundationDB's actual pattern** instead of three-actor design:
- **Single background task** instead of separate ConnectionKeeper/Reader/Writer
- **Synchronous send()** instead of async channel send
- **Trigger coordination** instead of complex channel orchestration
- **Immediate queueing** instead of async message passing

### Current State  
The Peer implementation now matches FoundationDB's network architecture with synchronous API and background actor handling all TCP I/O. All tests pass (104/104) and RefCell borrow conflicts are eliminated.

## Queue & Connection Assertions: ✅ COMPLETED

### Sometimes Assertions Fully Integrated
All queue and connection assertions have been restored and are actively working:

**Queue-related assertions in `peer/core.rs`:**
- ✅ `peer_queue_grows` - Message queue sometimes contains multiple messages
- ✅ `peer_queue_near_capacity` - Message queue sometimes approaches capacity limit  
- ✅ `peer_requeues_on_failure` - Peer sometimes re-queues messages after send failure
- ✅ `peer_recovers_after_failures` - Peer sometimes successfully connects after previous failures

**Strategic buggify placement**: Coordinated with PeerConfig tuning (queue sizes 2-6, burst sizes 5-15) to naturally trigger all assertions during chaos testing.

**Validation**: All assertions trigger reliably with `UntilAllSometimesReached(1000)` iteration control.

## Phase 10.1: Enhancements (COMPLETED)

### Completed Items
- [x] **Clean clippy warnings**: Removed unnecessary `#[allow(dead_code)]` annotations where fields are actually used
- [x] **Improve reading notifications**: Implemented truly event-driven reading using FDB pattern with `std::future::pending()` when no connection exists
- [x] **Factorize assertion validation**: Extracted common validation logic into `panic_on_assertion_violations()` helper function in assertions module
- [x] **Add TaskProvider::yield_now()**: Added abstracted yield method to TaskProvider trait for proper async coordination