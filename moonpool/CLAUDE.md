# Moonpool: Distributed Actor System

## Overview
Actor system crate built on `moonpool-foundation`, providing Orleans-inspired distributed actors with location transparency and automatic lifecycle management.

**Status**: ⚠️ **EARLY ALPHA - FUNCTIONAL BUT EXPERIMENTAL**

Core features implemented and working, but lacks production hardening, comprehensive integration tests, and monitoring. Use for hobby projects only.

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

## Current Implementation Status

### ✅ Fully Implemented

#### 1. Actor Base Trait (`actor/traits.rs` - 414 lines)
**Purpose**: Define actor interface

```rust
#[async_trait(?Send)]
trait Actor {
    type State: Serialize + DeserializeOwned;

    fn actor_id(&self) -> &ActorId;
    async fn on_activate(&mut self, state: Option<Self::State>) -> Result<()>;
    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<()>;

    // Handler registration
    fn register_handlers(&self, registry: &mut HandlerRegistry<Self>);
}
```

**Features**:
- Lifecycle hooks (on_activate, on_deactivate)
- Handler registration for dynamic message dispatch
- Placement hints for actor placement control
- Deactivation policies (OnExplicitDeactivation, DeactivateOnIdle)

#### 2. ActorContext (`actor/context.rs` - 821 lines)
**Purpose**: Per-actor execution environment

**Components**:
- Message channel for actor messages
- Control channel for lifecycle management
- State management with storage binding
- Error tracking and isolation
- Handler registry for message dispatch

**Message Processing Loop**:
```rust
loop {
    tokio::select! {
        msg = rx.recv() => {
            // Process actor message
            dispatch_to_handler(msg).await;
        }
        ctrl = ctrl_rx.recv() => {
            // Handle deactivation
            if let ControlMessage::Deactivate = ctrl {
                break;
            }
        }
    }
}
```

**Features**:
- Single-threaded message processing
- Error isolation per actor
- Automatic reactivation support
- Deactivation guard prevents new messages during shutdown

#### 3. ActorCatalog (`actor/catalog.rs` - 1,278 lines)
**Purpose**: Local actor registry with lifecycle management

**Pattern**: Double-check locking for race-free activation

```rust
// 1. Check if exists (fast path)
if let Some(ctx) = catalog.actors.get(&actor_id) {
    return Ok(ctx);
}

// 2. Acquire lock
let mut actors = catalog.actors.lock();

// 3. Check again (race handling)
if let Some(ctx) = actors.get(&actor_id) {
    return Ok(ctx);
}

// 4. Create actor via factory
let actor = factory.create(actor_id).await?;
```

**Features**:
- Factory-based auto-activation
- Directory integration for location tracking
- State persistence loading on activation
- Comprehensive unit tests for race conditions

**Reference**: Orleans `Catalog.cs`, `ActivationData.cs`

#### 4. MessageBus (`messaging/bus.rs` - 1,156 lines)
**Purpose**: Route messages to local/remote actors

**Components**:
- Catalog for local actor lookup
- Directory for location discovery
- Network transport for remote messaging
- CallbackManager for request-response correlation

**Routing Logic**:
```rust
async fn route_message(&self, target: ActorId, msg: Message) {
    // 1. Check directory for actor location
    if let Some(node) = directory.lookup(&target).await? {
        if node == self.node_id {
            // Local delivery
            self.catalog.deliver_local(target, msg).await?;
        } else {
            // Remote delivery
            self.transport.send_to(node, msg).await?;
        }
    } else {
        // Use placement hint to select node
        let node = placement.select_node(&target).await?;
        directory.register(&target, node).await?;
        // ... route to selected node
    }
}
```

**Features**:
- Location transparency
- Placement hints for actor placement
- Remote forwarding to other nodes
- Local dispatch optimization
- Request-response correlation with timeouts
- One-way messages (fire-and-forget)

**Reference**: Orleans `MessageCenter.cs`, `InsideRuntimeClient.cs`

#### 5. CallbackManager (`messaging/correlation.rs` - 729 lines)
**Purpose**: Correlation ID generation and response tracking

**Features**:
- Monotonic correlation ID generation
- Pending request tracking with oneshot channels
- Timeout handling
- Response completion

**Reference**: Orleans `CallbackData.cs`, `ResponseCompletionSource.cs`

#### 6. ActorRuntime (`runtime/actor_runtime.rs` - 481 lines)
**Purpose**: Multi-node runtime coordination

**Initialization**:
```rust
let runtime = ActorRuntime::builder()
    .namespace("example")
    .listen_addr("127.0.0.1:5000")?
    .cluster_nodes(vec![node1, node2, node3])
    .shared_directory(directory)
    .with_storage(storage)
    .with_providers(network, time, task)
    .with_serializer(JsonSerializer)
    .build()
    .await?;
```

**Features**:
- Namespace support for logical isolation
- ServerTransport integration for network listening
- Actor registration with factories
- Message reception loop
- Graceful shutdown hooks (stub implementation)

#### 7. Directory (`directory/simple.rs` - 420 lines)
**Purpose**: Actor location tracking

**Interface**:
```rust
#[async_trait(?Send)]
trait Directory {
    async fn register(&self, actor_id: &ActorId, node_id: NodeId) -> Result<()>;
    async fn lookup(&self, actor_id: &ActorId) -> Result<Option<NodeId>>;
    async fn unregister(&self, actor_id: &ActorId) -> Result<()>;
}
```

**Implementation**: SimpleDirectory with in-memory HashMap

**Reference**: Orleans `ActivationDirectory.cs`

#### 8. Placement (`placement/simple.rs` - 319 lines)
**Purpose**: Actor placement decisions

**Strategies**:
- **Random**: Random node selection
- **Local**: Prefer local node
- **LeastLoaded**: Select node with fewest actors

**Reference**: Orleans placement strategies

#### 9. Storage (`storage/memory.rs` - varies)
**Purpose**: State persistence

**Interface**:
```rust
#[async_trait(?Send)]
trait StorageProvider {
    async fn load(&self, actor_id: &ActorId) -> Result<Option<Vec<u8>>>;
    async fn save(&self, actor_id: &ActorId, data: Vec<u8>) -> Result<()>;
}
```

**Implementations**:
- InMemoryStorage for testing
- Extensible for databases (Redis, PostgreSQL, etc.)

#### 10. Serialization (`serialization/mod.rs` - 311 lines)
**Purpose**: Pluggable message/state serialization

**Default**: JSON via `serde_json`
**Alternatives**: Protobuf, Bincode, MessagePack (user choice)

**Pattern**:
```rust
trait Serializer {
    fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>>;
    fn deserialize<T: DeserializeOwned>(&self, data: &[u8]) -> Result<T>;
}
```

### ⚠️ Partially Implemented

- **Idle Timeout**: Policy exists (`DeactivateOnIdle`) but timeout logic not fully implemented
- **Graceful Shutdown**: Stub implementation with TODO comment
- **Integration Tests**: Limited simulation-based testing

### ❌ Not Implemented

- **Time-based Idle Timeout**: Needs integration with TimeProvider
- **Replication/Backup**: Not planned
- **Monitoring/Metrics**: No telemetry yet
- **Production Hardening**: Error paths need review

## Architecture Components

```
ActorRuntime
  ├─ ActorCatalog (local registry)
  │    └─ ActorContext (per-actor state + message loop)
  ├─ MessageBus (routing + correlation)
  │    └─ CallbackManager (request-response tracking)
  ├─ Directory (location tracking)
  ├─ Placement (placement decisions)
  ├─ Storage (state persistence)
  └─ ServerTransport (network listening)
```

## Testing Strategy

### Unit Tests (76 passing)

**Coverage**:
- ActorCatalog: Factory creation, race handling, directory integration
- MessageBus: Routing, correlation, local/remote dispatch
- CallbackManager: Correlation IDs, timeouts, completion
- Message: Envelope serialization, flags, direction
- Directory: Registration, lookup, unregistration
- Placement: Random, Local, LeastLoaded strategies
- Storage: Save, load, overwrite, clear
- Serialization: JSON round-trip, invalid data

**Run tests**:
```bash
nix develop --command cargo nextest run -p moonpool
```

### Integration Tests (Limited)

**Working Examples**:
- `hello_actor`: Multi-node virtual actor demonstration
- `bank_account`: State persistence with DeactivateOnIdle

**Run examples**:
```bash
# Validate multi-node functionality
nix develop --command cargo run --example hello_actor -- --nodes 2
nix develop --command cargo run --example bank_account -- --nodes 2
```

**Before completing any work**: Run all examples to ensure they execute without panics

### Simulation Testing (Future)

Orleans-inspired actors should be testable with moonpool-foundation simulation:

```rust
async fn actor_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
) -> SimulationResult<SimulationMetrics> {
    let runtime = ActorRuntime::with_providers(
        "test",
        network,
        time,
        task_provider,
    ).await?;

    // Run deterministic workload...
    Ok(SimulationMetrics::default())
}
```

## Orleans Reference Architecture

We follow Orleans patterns with clearer naming:

| Orleans | Moonpool | Purpose |
|---------|----------|---------|
| Grain | Actor | Core compute unit |
| GrainDirectory | Directory | Location tracking |
| MessageCenter | MessageBus | Message routing |
| Catalog | ActorCatalog | Local registry |
| ActivationData | ActorContext | Per-actor state |
| Silo | ActorRuntime | Node runtime |

### Orleans Reference Code

**Location**: `../docs/references/orleans/`

**When working on**:
- **ActorCatalog**: Read `Catalog.cs`, `ActivationDirectory.cs`, `ActivationData.cs`
- **MessageBus**: Read `Message.cs`, `CallbackData.cs`, `ResponseCompletionSource.cs`
- **Lifecycle**: Read `Grain.cs`, `IGrain.cs`, `DeactivationReason.cs`
- **Placement**: Read placement strategy implementations

**Analysis Documents**: `../docs/analysis/orleans/`

## Design Principles

1. **Orleans Architecture, Clearer Names**: Same patterns, better vocabulary
2. **Pluggable Serialization**: `Vec<u8>` supports any format (default: JSON)
3. **Location Transparency**: ActorRef hides physical location
4. **Automatic Activation**: Actors created on first message
5. **State Persistence**: ActorState wrapper with storage integration
6. **Built on Foundation**: Uses moonpool-foundation for networking

## Validation Checklist

Before completing any work:
1. `nix develop --command cargo fmt` - passes
2. `nix develop --command cargo clippy` - no warnings
3. `nix develop --command cargo nextest run -p moonpool` - all tests pass
4. Run both examples successfully without panics
5. Documentation updated for public APIs

## Known Issues

- **Graceful Shutdown**: Stub implementation needs completion (runtime/actor_runtime.rs:471-476)
- **Idle Timeout**: Policy exists but timeout logic not implemented
- **Integration Tests**: Needs more comprehensive simulation testing
- **Monitoring**: No metrics or telemetry
- **Production Hardening**: Error handling needs review for edge cases

## Notes

- Built on `moonpool-foundation` - always use provider traits
- Focus on Orleans patterns, not Orleans vocabulary
- Early alpha - functional but experimental
- Examples serve as integration tests
- Use specify-cli workflow for development iterations
