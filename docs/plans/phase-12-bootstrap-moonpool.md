# Bootstrap Actor System Implementation Plan

## Overview
Build a distributed actor system in Rust, inspired by Orleans but without Orleans-specific vocabulary. Start with single-process static actors, gradually add networking and virtual actors.

## Core Concepts

### ActorId Structure
```rust
ActorId {
    namespace: String,
    actor_type: String,
    key: Option<String>,  // None = static actor, Some = virtual actor
}
```
- **Static actors**: Pre-created, always exist (key = None)
- **Virtual actors**: Created on-demand, can be deactivated (key = Some)

## Phase 1: Local Actor System

### Step 1: Basic ActorCatalog
**Orleans Reference**: `Catalog.cs`, `GrainId.cs`, `ActivationData.cs`

**Goal**: Local actor registry with activation

**Components**:
- `ActorCatalog`: HashMap of ActorId → ActorInstance
- Methods: `get_or_create()`, `get()`, `remove()`, `exists()`

**Actor State Machine**:
```
[NotExists] → [Creating] → [Active] → [Deactivating] → [NotExists]
```

#### Simulation Strategy

**Invariants** (run after every event):
- BUG DETECTOR: No actor exists in multiple states simultaneously (catches state machine bugs)
- BUG DETECTOR: Total active count = sum of per-catalog counts (catches accounting bugs)

**Critical Assertions**:
- Always: Actor in NotExists state never appears in catalog
- Always: Only Deactivating actors can be removed
- Sometimes: Concurrent creation races occur (validates lock safety)
- Sometimes: Lookups for non-existent actors occur (validates error paths)

**Buggify Injection Points**:
- Before lock acquisition in get_or_create() - inject 1-100μs delay (exposes race conditions)
- Before state transitions - inject 1ns delay (tests atomicity)

**StateRegistry Exposure**:
- Export: active_count, per-actor state, creation timestamps
- Why: Enables invariant checking across all catalog instances

**Test Scenarios**:
- Concurrent creation: 10 workloads creating same actor simultaneously (validates uniqueness)
- Rapid churn: Create/delete cycles (validates cleanup)
- Lookup storms: Mass lookups of non-existent actors (validates error handling)

**Topologies**: 1x1 (basic), 2x2 (independence), 10x10 (stress)

### Step 2: ActorCatalog State Machine
**Orleans Reference**: `Silo.cs`, `SiloLifecycle.cs`

**Goal**: Catalog lifecycle management

**State Machine**:
```
[Initializing] → [Running] → [Stopping] → [Stopped]
```

#### Simulation Strategy

**Invariants**:
- BUG DETECTOR: No catalog in invalid state (validates state transitions)
- BUG DETECTOR: Operations only occur in Running state (validates lifecycle correctness)

**Critical Assertions**:
- Always: start() called only from Initializing state
- Always: stop() called only from Running or Stopping state
- Always: Messages handled only when Running
- Sometimes: Shutdown during operations (validates graceful degradation)
- Sometimes: Rapid start/stop cycles (validates lifecycle robustness)

**Buggify Injection Points**:
- In start() - inject 1-50ms delay (tests slow initialization)
- Before state transitions - inject 100-1000μs delay (tests race conditions)

**StateRegistry Exposure**:
- Export: catalog_state, operations_count, start_time, stop_time
- Why: Track lifecycle progression and operation validity

**Test Scenarios**:
- Rapid lifecycle: Start/stop every 10ms (validates transition correctness)
- Operation flood during shutdown: 1000 ops while stopping (validates queue draining)
- Delayed initialization: Random 0-100ms startup (validates timeout handling)

**Topologies**: 1x1 (basic), 2x2 (concurrent lifecycles)

### Step 3: Actor Base Trait
**Orleans Reference**: `Grain.cs`, `IGrain.cs`, `Grain{TState}.cs`

**Goal**: Define actor interface

```rust
#[async_trait]
trait Actor: Send + Sync {
    async fn on_activate(&mut self, ctx: &ActorContext) -> Result<()>;
    async fn on_deactivate(&mut self, ctx: &ActorContext) -> Result<()>;
    async fn handle(&mut self, msg: Message, ctx: &ActorContext) -> Result<Response>;
}
```

#### Simulation Strategy

**Invariants**:
- BUG DETECTOR: Deactivations ≤ Activations (catches double-deactivation bugs)
- BUG DETECTOR: Messages only handled after activation (validates lifecycle ordering)

**Critical Assertions**:
- Always: handle() called only when activated
- Always: on_deactivate() called only when no pending messages
- Sometimes: Activation failures occur (validates error recovery)
- Sometimes: Message handling errors occur (validates error path coverage)
- Sometimes: Rapid activation/deactivation cycles (validates lifecycle stress)

**Buggify Injection Points**:
- In on_activate() - return error 25% of time (validates activation failure handling)
- In handle() - inject 1-100ms delay 10% of time (validates timeout logic)

**StateRegistry Exposure**:
- Export: is_activated, activation_count, deactivation_count, messages_handled
- Why: Track actor lifecycle and message processing correctness

**Test Scenarios**:
- Activation storms: 100 actors activated simultaneously (validates resource limits)
- Message floods: 1000 messages to single actor (validates queue handling)
- Rapid lifecycle: Activate, 1 message, deactivate repeatedly (validates cleanup)

**Topologies**: 1x1 (basic), 10x1 (many actors)

### Step 4: Actor Inbox State Machine
**Orleans Reference**: `WorkItemGroup.cs`, `ActivationTaskScheduler.cs`, `WorkQueue.cs`

**Goal**: Message queue per actor

**State Machine**:
```
[Empty] → [HasMessages] → [Paused] → [Draining]
```
- Each actor gets `mpsc::Receiver<Message>`
- Single-threaded message processing

#### Simulation Strategy

**Invariants**:
- BUG DETECTOR: Message conservation: sent = received + queued (catches message loss/duplication)
- BUG DETECTOR: Queue state matches queue size (Empty → 0 messages, HasMessages → >0 messages)

**Critical Assertions**:
- Always: Queue size < max_capacity (catches overflow bugs)
- Always: Queue state consistent with contents
- Sometimes: Queue near capacity (validates back-pressure)
- Sometimes: Queue draining (validates graceful shutdown)
- Sometimes: Queue paused (validates flow control)

**Buggify Injection Points**:
- Before enqueue - inject 1-1000ns delay (tests enqueue races)
- During dequeue - inject 10-500μs delay 5% of time (simulates slow processing)

**StateRegistry Exposure**:
- Export: queue_state, messages_queued, messages_sent, messages_received, max_capacity, utilization
- Why: Track message flow and queue pressure

**Test Scenarios**:
- Queue overflow: Send 1000 messages faster than processing (validates back-pressure)
- Burst traffic: Alternating floods and quiet periods (validates queue dynamics)
- Pause/resume: Random pause during processing (validates flow control)

**Topologies**: 1x1 (basic), 2x2 (independence), 10x1 (many producers)

### Step 5: Message Processing State Machine
**Orleans Reference**: `Message.cs`, `MessageStatistics.cs`

**Goal**: Track message lifecycle

**State Machine**:
```
[Queued] → [Processing] → [Completed|Failed|Timeout]
```

#### Simulation Strategy

**Invariants**:
- BUG DETECTOR: Valid state transitions only (Queued → Processing → Terminal states)
- BUG DETECTOR: Completed + Failed + Timeout ≤ Sent (catches message creation bugs)

**Critical Assertions**:
- Always: Processing state only from Queued
- Always: Completion only from Processing state
- Sometimes: Message failures occur (validates error handling)
- Sometimes: Message timeouts occur (validates timeout logic)
- Sometimes: Rapid completions (<1ms) (validates fast path)

**Buggify Injection Points**:
- In process_message() - return error 25% of time (validates failure handling)
- Before completion - inject 1-10s delay 5% of time (triggers timeouts)

**StateRegistry Exposure**:
- Export: sent_count, completed_count, failed_count, timeout_count, per-message state/duration
- Why: Track message lifecycle and identify stuck messages

**Test Scenarios**:
- Timeout storms: Random 1-10s delays triggering timeouts (validates timeout recovery)
- Failure injection: 20% random failure rate (validates error path coverage)
- Processing delays: Variable 0-100ms processing (validates performance under load)

**Topologies**: 1x1 (basic), 2x2 (concurrent processing)

## Phase 2: MessageBus (Local Routing)

### Step 6: MessageBus Implementation
**Orleans Reference**: `MessageCenter.cs`, `InsideRuntimeClient.cs`

**Goal**: Route messages to local actors

**Components**:
```rust
Message {
    target: ActorId,
    payload: Vec<u8>,
    reply_to: Option<oneshot::Sender>,
}

MessageBus {
    catalog: Arc<ActorCatalog>,
}
```

**Serialization**: The `payload: Vec<u8>` field enables pluggable serialization. Moonpool will use `serde_json` initially for simplicity and debugging, but users can swap to any format (protobuf, bincode, messagepack, etc.) by implementing their own envelope serialization layer.

**Bus State Machine**:
```
[Starting] → [Ready] → [Degraded] → [Stopping]
```

#### Simulation Strategy

**Invariants**:
- BUG DETECTOR: Routing conservation: sent = delivered + in_transit (catches message loss)
- BUG DETECTOR: Delivery only when Ready (validates state enforcement)

**Critical Assertions**:
- Always: Route only when bus is Ready
- Always: Deliver only to existing actors in catalog
- Sometimes: Routing to non-existent actor (validates error handling)
- Sometimes: Bus in Degraded state (validates degradation recovery)
- Sometimes: Routing during shutdown (validates graceful termination)

**Buggify Injection Points**:
- Before catalog lookup - inject 1-500μs delay (tests lookup races)
- Before delivery - transition to Degraded 10% of time, sleep 10-100ms, return to Ready (validates state transitions)

**StateRegistry Exposure**:
- Export: bus_state, messages_sent, messages_delivered, in_transit_count, routing_errors, avg_latency
- Why: Monitor bus health and routing correctness

**Test Scenarios**:
- Actor churn: Create/destroy actors during routing (validates lookup correctness)
- Routing storms: 1000 simultaneous routes (validates concurrency)
- Degraded mode: Random transitions to Degraded (validates recovery)

**Topologies**: 1x1 (basic), 1x10 (one bus, many senders)

### Step 7: Request/Response Pattern
**Orleans Reference**: `CallbackData.cs`, `ResponseCompletionSource.cs`

**Goal**: Async request/response

**State Machine**:
```
[Sent] → [Received] → [Executing] → [Responded|TimedOut|Failed]
```

#### Simulation Strategy

**Invariants**:
- BUG DETECTOR: Responses ≤ Requests (catches duplicate response bugs)
- BUG DETECTOR: Correlation ID uniqueness (catches ID collision bugs)

**Critical Assertions**:
- Always: Unique correlation IDs (no duplicates in pending requests)
- Always: Responses match pending requests (correlation ID exists)
- Sometimes: Request timeouts (validates timeout handling)
- Sometimes: Rapid turnaround (<1ms) (validates fast path)
- Sometimes: Many concurrent requests (>10 pending) (validates concurrency)

**Buggify Injection Points**:
- During request execution - inject 1-10s delay 25% of time (triggers timeouts)
- Before sending response - return error 5% of time (validates error recovery)

**StateRegistry Exposure**:
- Export: requests_sent, responses_received, timeouts, failures, pending_requests (with correlation IDs)
- Why: Track request/response pairing and timeout behavior

**Test Scenarios**:
- Timeout storms: Random execution delays 1-10s (validates timeout recovery)
- Request floods: 1000 concurrent requests (validates correlation ID management)
- Intermittent failures: 20% random failure rate (validates error propagation)

**Topologies**: 1x1 (basic), 2x2 (distributed req/resp), 10x10 (stress)

### Step 8: Virtual Actor Lifecycle
**Orleans Reference**: `ActivationData.cs`, `GrainCollectionOptions.cs`, `DeactivationReason.cs`

**Goal**: On-demand activation, idle timeout

**State Machine**:
```
[Cold] → [Warming] → [Hot] → [Cooling] → [Cold]
```

#### Simulation Strategy

**Invariants**:
- BUG DETECTOR: Hot actors have recent activity (idle_time < idle_timeout)
- BUG DETECTOR: Cold actors have no queued messages

**Critical Assertions**:
- Always: Hot transition only from Warming
- Always: Cold transition only when message queue empty
- Sometimes: Idle timeout triggers (validates timeout logic)
- Sometimes: Reactivation from Cold (validates on-demand activation)
- Sometimes: Sustained Hot state (>60s with continuous activity)

**Buggify Injection Points**:
- During warming - inject 100-5000ms delay (validates activation timeouts)
- During idle check - randomize timeout ±20% (validates timeout jitter handling)

**StateRegistry Exposure**:
- Export: per-actor state, last_activity_ms, idle_timeout_ms, queue_length, activation/deactivation counts
- Why: Monitor virtual actor lifecycle and idle timeout correctness

**Test Scenarios**:
- Idle timeout stress: Random activity patterns triggering timeouts (validates timeout enforcement)
- Activation storms: 100 cold actors activated simultaneously (validates resource handling)
- Hot/cold cycles: Activity bursts with long idle periods (validates lifecycle transitions)

**Topologies**: 1x1 (basic), 10x1 (many actors, one manager)

---

## Testing Philosophy

Every step follows the FoundationDB approach:

**Core Principles**:
- **100% Deterministic**: Fixed seeds for reproducibility
- **Chaos by Default**: `use_random_config()` enables network faults
- **Comprehensive Coverage**: `UntilAllSometimesReached(1000+)` ensures all error paths tested
- **Early Crash**: Invariants run after EVERY event
- **Multi-Topology**: Test 1x1 (basic), 2x2 (distributed), 10x10 (stress)

**Success Criteria Per Step**:
1. All unit tests pass
2. Simulation tests achieve 100% success rate over 1000+ iterations
3. All `sometimes_assert!` statements triggered at least once
4. No assertion violations
5. No deadlocks or infinite loops
6. Multi-topology tests pass

## Key Design Decisions

1. **State Machines Everywhere**: Every component has explicit states for debugging and testing
2. **Simulation-First**: Built on moonpool's deterministic testing framework
3. **Orleans Concepts, Different Names**: Actor instead of Grain, but same architecture
4. **Pluggable Serialization**: Message payloads use `Vec<u8>` to support any serialization format. Moonpool starts with `serde_json` for simplicity and human-readable debugging, but users can swap to optimized formats (protobuf for schema evolution, bincode for speed, messagepack for size) by implementing custom envelope serializers without changing the core actor system