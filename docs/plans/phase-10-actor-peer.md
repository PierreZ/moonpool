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

### Current State
The Peer now has TaskProvider capability but still uses synchronous request-response pattern. The infrastructure is ready for actor-based implementation using message passing to avoid RefCell borrow conflicts.

## Next Phase: Channel-based Actor Implementation

### Phase 1: Message Passing Architecture
Replace shared RefCell state with mpsc channels to avoid async borrow conflicts:

```rust
pub struct Peer<N: NetworkProvider, T: TimeProvider, TP: TaskProvider> {
    // Actor handles
    keeper_handle: Option<JoinHandle<()>>,
    
    // Channel-based communication (no RefCell conflicts)
    send_tx: mpsc::UnboundedSender<Vec<u8>>,
    receive_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    
    // Simple shutdown signaling
    shutdown_tx: mpsc::UnboundedSender<()>,
    
    config: PeerConfig,
    task_provider: TP,
}
```

### Phase 2: Actor Functions
- `connection_keeper_actor()` - Connection lifecycle management
- `connection_reader_actor()` - Background TCP reading  
- `connection_writer_actor()` - Background TCP writing

### Phase 3: API Transformation
- `Peer::send()` remains async but becomes channel send (no TCP I/O blocking)
- `Peer::receive()` remains async but becomes channel receive 
- All actors spawn in `Peer::new()`
- **Keep familiar tokio patterns** - developers expect async network APIs

### Phase 4: Restore Queue & Connection Sometimes Assertions
Re-enable commented out `sometimes_assert!` calls to ensure comprehensive chaos testing coverage:

**Queue-related assertions in `peer/core.rs`:**
- `peer_queue_grows` - Message queue should sometimes contain multiple messages
- `peer_queue_near_capacity` - Message queue should sometimes approach capacity limit  
- `peer_requeues_on_failure` - Peer should sometimes re-queue messages after send failure
- `peer_recovers_after_failures` - Peer should sometimes successfully connect after previous failures

**Purpose**: Validate that chaos testing exercises all queue management and connection recovery code paths during actor-based operation.