# Moonpool: Distributed Actor System

## Overview
Actor system crate built on `moonpool-foundation`, providing Orleans-inspired distributed actors with deterministic testing support.

**Status**: Phase 12 IN PROGRESS - Actor system bootstrap

## Relationship to Foundation
This crate builds on `moonpool-foundation` which provides:
- Core simulation infrastructure (SimWorld, event processing)
- Sans I/O transport layer (ClientTransport, ServerTransport)
- Provider pattern (Network, Time, Task, Random)
- Testing framework (buggify, assertions, invariants)
- Multi-topology testing infrastructure

**Use foundation's APIs**:
- `NetworkProvider` for connections
- `TimeProvider::sleep()` instead of `tokio::time::sleep()`
- `TaskProvider::spawn_task()` instead of `tokio::spawn()`
- `ClientTransport::request()` for type-safe messaging
- `ServerTransport::try_next_message()` for receiving messages

## Core Concepts

### ActorId Structure
```rust
ActorId {
    namespace: String,      // Logical grouping
    actor_type: String,     // Type of actor
    key: Option<String>,    // None = static, Some = virtual
}
```

- **Static actors**: Pre-created, always exist (key = None)
- **Virtual actors**: Created on-demand, can be deactivated (key = Some)

### Actor Lifecycle
```
[NotExists] → [Creating] → [Active] → [Deactivating] → [NotExists]
```

## Architecture Components

### 1. ActorCatalog (Phase 12, Step 1-2)
**Purpose**: Local actor registry with lifecycle management

**State Machine**:
```
Catalog: [Initializing] → [Running] → [Stopping] → [Stopped]
Actor: [NotExists] → [Creating] → [Active] → [Deactivating] → [NotExists]
```

**Core Operations**:
- `get_or_create(ActorId)` - Atomic actor creation with race handling
- `get(ActorId)` - Lookup existing actor
- `remove(ActorId)` - Deactivate and remove actor
- `exists(ActorId)` - Check actor presence

**Invariants** (run after every simulation event):
- BUG DETECTOR: No actor in multiple states simultaneously
- BUG DETECTOR: Total active count = sum of per-catalog counts
- Always: Actor in NotExists never in catalog
- Always: Only Deactivating actors can be removed

**Buggify Placement**:
- Before lock acquisition: 1-100μs delay (exposes race conditions)
- Before state transitions: 1ns delay (tests atomicity)

**Reference**: Orleans `Catalog.cs`, `ActivationData.cs`, `SiloLifecycle.cs`

### 2. Actor Base Trait (Phase 12, Step 3)
**Purpose**: Define actor interface

```rust
#[async_trait(?Send)]
trait Actor {
    async fn on_activate(&mut self, ctx: &ActorContext) -> Result<()>;
    async fn on_deactivate(&mut self, ctx: &ActorContext) -> Result<()>;
    async fn handle(&mut self, msg: Message, ctx: &ActorContext) -> Result<Response>;
}
```

**Invariants**:
- BUG DETECTOR: Deactivations ≤ Activations (catches double-deactivation)
- BUG DETECTOR: Messages only handled after activation
- Always: handle() called only when activated
- Always: on_deactivate() called only when no pending messages

**Buggify Placement**:
- In on_activate(): return error 25% of time
- In handle(): inject 1-100ms delay 10% of time

**Reference**: Orleans `Grain.cs`, `IGrain.cs`

### 3. Actor Inbox (Phase 12, Step 4)
**Purpose**: Message queue per actor with flow control

**State Machine**:
```
[Empty] → [HasMessages] → [Paused] → [Draining]
```

**Implementation**:
- Each actor has `mpsc::Receiver<Message>`
- Single-threaded message processing
- Back-pressure support

**Invariants**:
- BUG DETECTOR: Message conservation (sent = received + queued)
- BUG DETECTOR: Queue state matches size (Empty → 0, HasMessages → >0)
- Always: Queue size < max_capacity

**Buggify Placement**:
- Before enqueue: 1-1000ns delay (tests races)
- During dequeue: 10-500μs delay 5% of time (simulates slow processing)

**Reference**: Orleans `WorkItemGroup.cs`, `ActivationTaskScheduler.cs`, `WorkQueue.cs`

### 4. Message Processing (Phase 12, Step 5)
**Purpose**: Track message lifecycle

**State Machine**:
```
[Queued] → [Processing] → [Completed|Failed|Timeout]
```

**Invariants**:
- BUG DETECTOR: Valid state transitions only
- BUG DETECTOR: Completed + Failed + Timeout ≤ Sent
- Always: Processing only from Queued
- Always: Completion only from Processing

**Buggify Placement**:
- In process_message(): return error 25% of time
- Before completion: inject 1-10s delay 5% of time (triggers timeouts)

**Reference**: Orleans `Message.cs`, `MessageStatistics.cs`

### 5. MessageBus (Phase 12, Step 6-7)
**Purpose**: Route messages to local actors

**Components**:
```rust
Message {
    target: ActorId,
    payload: Vec<u8>,              // Pluggable serialization
    reply_to: Option<oneshot::Sender>,
}

MessageBus {
    catalog: Arc<ActorCatalog>,
}
```

**Serialization**: Uses `Vec<u8>` for pluggable formats:
- Default: `serde_json` (debugging, simplicity)
- Alternatives: protobuf, bincode, messagepack (user choice)

**Bus State Machine**:
```
[Starting] → [Ready] → [Degraded] → [Stopping]
```

**Request/Response Pattern**:
```
[Sent] → [Received] → [Executing] → [Responded|TimedOut|Failed]
```

**Invariants**:
- BUG DETECTOR: Routing conservation (sent = delivered + in_transit)
- BUG DETECTOR: Delivery only when Ready
- BUG DETECTOR: Responses ≤ Requests
- BUG DETECTOR: Correlation ID uniqueness
- Always: Route only when Ready
- Always: Deliver only to existing actors
- Always: Unique correlation IDs

**Buggify Placement**:
- Before catalog lookup: 1-500μs delay (tests races)
- Before delivery: Degraded state 10% of time (10-100ms), then Ready
- During request execution: 1-10s delay 25% of time (triggers timeouts)

**Reference**: Orleans `MessageCenter.cs`, `InsideRuntimeClient.cs`, `CallbackData.cs`, `ResponseCompletionSource.cs`

### 6. Virtual Actor Lifecycle (Phase 12, Step 8)
**Purpose**: On-demand activation with idle timeout

**State Machine**:
```
[Cold] → [Warming] → [Hot] → [Cooling] → [Cold]
```

**Invariants**:
- BUG DETECTOR: Hot actors have recent activity (idle_time < idle_timeout)
- BUG DETECTOR: Cold actors have no queued messages
- Always: Hot transition only from Warming
- Always: Cold transition only when queue empty

**Buggify Placement**:
- During warming: 100-5000ms delay (validates activation timeouts)
- During idle check: randomize timeout ±20% (validates jitter handling)

**Reference**: Orleans `ActivationData.cs`, `GrainCollectionOptions.cs`, `DeactivationReason.cs`

## Testing Strategy

### State Machines Everywhere
Every component has explicit states for debugging and deterministic testing:
- ActorCatalog lifecycle
- Actor lifecycle
- Inbox states
- Message processing states
- Bus states
- Virtual actor activation states

### Simulation-First Development
Built on foundation's deterministic testing:
1. Write component with state machine
2. Define invariants (cross-workload properties)
3. Add always_assert! (per-component validation)
4. Add sometimes_assert! (error path coverage)
5. Place buggify strategically
6. Test with `UntilAllSometimesReached(1000)`

### Success Criteria Per Step
1. All unit tests pass
2. Simulation tests achieve 100% success rate over 1000+ iterations
3. All `sometimes_assert!` triggered at least once
4. No assertion violations
5. No deadlocks or infinite loops
6. Multi-topology tests pass (1x1, 2x2, 10x10)

### Multi-Topology Testing
- **1x1**: Basic functionality validation
- **1x10**: One bus, many senders (concurrency)
- **2x2**: Distributed scenarios (independence)
- **10x1**: Many actors, one manager (resource limits)
- **10x10**: Stress testing (chaos conditions)

### Running Examples as Tests
Examples in the moonpool crate serve as integration tests and must be run to verify the system works correctly:

```bash
# Run specific example
nix develop --command cargo run --example bank_account

# Run all examples (recommended for validation)
nix develop --command cargo run --example bank_account
nix develop --command cargo run --example <other_example_name>
```

**Why examples matter**:
- Examples exercise the full actor runtime with real network transport
- They validate multi-node scenarios and state persistence
- They catch RefCell borrow issues that unit tests might miss
- They demonstrate correct usage patterns for users

**Before completing any work**, run all examples to ensure they execute without panics or errors.

## Orleans vs Moonpool Vocabulary

We use Orleans concepts but different names:
- **Actor** (not Grain) - Core compute unit
- **ActorCatalog** (not GrainDirectory) - Registry
- **MessageBus** (not MessageCenter) - Routing
- Same architecture, clearer names

## Reference Documentation

### Orleans References
**Location**: `docs/references/orleans/`

**When working on ActorCatalog**:
- Read `Catalog.cs` (actor registry)
- Read `ActivationDirectory.cs` (routing)
- Read `ActivationData.cs` (actor state)

**When working on Inbox**:
- Read `WorkItemGroup.cs` (message queue)
- Read `ActivationTaskScheduler.cs` (scheduling)

**When working on MessageBus**:
- Read `Message.cs` (message structure)
- Read `CallbackData.cs` (request/response correlation)

**When working on Lifecycle**:
- Read `Grain.cs` and `IGrain.cs` (actor interface)
- Read `GrainCollectionOptions.cs` (virtual actor config)
- Read `DeactivationReason.cs` (deactivation triggers)

### Analysis Documents
**Location**: `docs/analysis/orleans/`

- `activation-lifecycle.md` - Actor activation/deactivation patterns
- `message-system.md` - Messaging architecture
- `task-scheduling.md` - Message processing and scheduling
- `configuration-policies.md` - Configuration patterns

### Implementation Plan
**Location**: `docs/plans/`

- `phase-12-bootstrap-moonpool.md` - Current phase detailed plan

## Design Principles

1. **State Machines Everywhere**: Explicit states for all components
2. **Simulation-First**: Built on foundation's deterministic testing
3. **Orleans Concepts, Different Names**: Same architecture, clearer vocabulary
4. **Pluggable Serialization**: `Vec<u8>` supports any format (default: serde_json)
5. **100% Deterministic**: Fixed seeds for reproducibility
6. **Chaos by Default**: Hostile infrastructure exposes bugs early

## Validation Checklist
Before completing any step:
1. State machine clearly defined
2. Invariants implemented in StateRegistry
3. always_assert! for component validation
4. sometimes_assert! for error path coverage
5. Buggify strategically placed
6. Multi-topology tests pass (1x1, 2x2, 10x10)
7. All sometimes_assert! triggered in chaos tests
8. All examples run successfully without panics
9. Documentation updated

## Notes
- Built on `moonpool-foundation` - always use provider traits
- Focus on Orleans patterns, not Orleans vocabulary
- Every component gets a state machine
- Invariants catch bugs that assertions miss
- Buggify exposes edge cases before production
