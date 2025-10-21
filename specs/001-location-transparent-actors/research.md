# Research: Location-Transparent Distributed Actor System

**Date**: 2025-10-21
**Phase**: 0 - Technical Research & Unknowns Resolution

## Executive Summary

This document resolves all technical unknowns identified in plan.md Phase 0, providing concrete implementation guidance for building a location-transparent distributed actor system on moonpool-foundation.

**Key Findings**:
- PeerTransport provides synchronous send + async receive with automatic reconnection
- Custom envelope protocol needed (not using RequestResponseProtocol)
- Buggify uses macros with probabilistic activation for deterministic chaos
- Providers injected via function parameters, stored in actor structs

---

## 1. PeerTransport API Research

### Decision: Use Peer + Custom Envelope Protocol

**Rationale**: The actor system requires custom addressing (ActorId, NodeId) and correlation semantics that differ from generic request-response. We'll build a custom envelope on PeerTransport's raw byte API.

### API Surface

#### Sending Messages
```rust
// From: moonpool-foundation/src/network/peer/core.rs

pub fn send(&mut self, data: Vec<u8>) -> PeerResult<()>
```
- **Synchronous**: Returns immediately after queuing
- **Non-blocking**: Background task handles TCP I/O
- **Automatic requeue**: Failures trigger reconnection and retry
- **Usage**: `peer.send(serialized_message)?;`

#### Receiving Messages
```rust
pub async fn receive(&mut self) -> PeerResult<Vec<u8>>
pub fn try_receive(&mut self) -> Option<Vec<u8>>
```
- **Async receive**: Blocks until data or disconnect
- **Non-blocking variant**: Returns immediately if no data
- **Usage**: `let data = peer.receive().await?;`

#### Connection Management
```rust
pub fn new(
    network: N,
    time: T,
    task_provider: TP,
    destination: String,
    config: PeerConfig,
) -> Self

pub fn is_connected(&self) -> bool
pub fn reconnect(&mut self)
pub async fn close(&mut self)
```
- **Automatic reconnection**: Exponential backoff on failures
- **Background actor**: Spawned on construction via TaskProvider
- **Configuration**: `PeerConfig::default()` or custom delays/limits

### Connection Establishment Pattern

**Background Actor Lifecycle**:
1. `Peer::new()` spawns connection task immediately (non-blocking)
2. Task attempts connection with exponential backoff
3. On success: processes send/receive queues
4. On failure: waits, then retries (up to max_connection_failures)
5. Shared state via `Rc<RefCell<PeerShared>>` (single-threaded)

**Configuration Options**:
```rust
pub struct PeerConfig {
    pub initial_reconnect_delay: Duration,      // Default: 100ms
    pub max_reconnect_delay: Duration,          // Default: 30s
    pub max_queue_size: usize,                  // Default: 1000 messages
    pub connection_timeout: Duration,           // Default: 5s
    pub max_connection_failures: Option<u32>,   // Default: None (unlimited)
}
```

**Presets**:
- `PeerConfig::local_network()` - LAN (10ms initial, 1s max)
- `PeerConfig::wan_network()` - WAN (500ms initial, 60s max)

### Message Format: Custom Actor Envelope

**Decision**: Build custom `ActorEnvelope` instead of using `RequestResponseEnvelope`

**Wire Format** (proposed):
```
[message_type: 1 byte]           // Request=1, Response=2, OneWay=3
[correlation_id: 8 bytes (u64)]  // For request-response matching
[target_actor_len: 4 bytes (u32)]
[target_actor: N bytes (UTF-8)]  // ActorId string
[sender_actor_len: 4 bytes (u32)]
[sender_actor: M bytes (UTF-8)]  // ActorId string
[payload_len: 4 bytes (u32)]
[payload: P bytes]               // serde_json serialized message
```

**Serialization Strategy**:
- Use `serde_json` for payload (human-readable, debuggable)
- Use little-endian binary for headers (efficient, deterministic)
- Implement `EnvelopeSerializer` trait for pluggability
- Support partial message buffering for streaming reception

### Provider Integration

**Pattern**: Peer generic over provider traits
```rust
struct Peer<N, T, TP>
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
{
    // ...
}

impl<N, T, TP> Peer<N, T, TP> {
    pub fn new(
        network: N,
        time: T,
        task_provider: TP,
        destination: String,
        config: PeerConfig,
    ) -> Self
}
```

**Key Insight**: Providers passed at construction, cloned internally for background tasks.

### Alternatives Considered

**RequestResponseProtocol**: Rejected because:
- Generic correlation only (no actor addressing)
- No support for OneWay messages
- Self-driving pattern unsuitable for actor mailboxes
- Actor system needs custom timeout semantics

---

## 2. Buggify Integration Research

### Decision: Use `buggify!()` and `buggify_with_prob!()` Macros

**Rationale**: Provides deterministic chaos injection with location-based activation tracking, enabling reproducible testing of race conditions and concurrent activation scenarios.

### API for Injecting Chaos Points

#### Initialization
```rust
// From: moonpool-foundation/src/buggify.rs

pub fn buggify_init(activation_prob: f64, firing_prob: f64)
pub fn buggify_reset()
```

**Usage Pattern**:
```rust
// At start of each simulation iteration
buggify_init(0.5, 0.25);  // 50% locations activate, 25% fire when active

// ... run simulation ...

// After iteration completes
buggify_reset();  // Clear all activation state
```

#### Injecting Chaos

**Default Probability** (25%):
```rust
if buggify!() {
    // Inject fault - 25% firing probability when activated
    return Err(DirectoryError::Timeout);
}
```

**Custom Probability**:
```rust
if buggify_with_prob!(0.1) {
    // 10% firing probability when activated
    time_provider.sleep(Duration::from_millis(500)).await?;
}
```

### How It Works: Two-Phase Activation

**Phase 1 - Location Activation** (once per iteration):
```rust
// First call to this source location
let is_active = sim_random::<f64>() < activation_prob;
// Store result: active_locations["file.rs:42"] = true/false
```

**Phase 2 - Probabilistic Firing** (each call):
```rust
// Subsequent calls to same location
if is_active && sim_random::<f64>() < firing_prob {
    return true;  // Chaos injected!
}
```

**Location Identification**: `concat!(file!(), ":", line!())` provides unique key per source location.

### Coverage Tracking

**Internal State** (thread-local):
```rust
thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

struct State {
    enabled: bool,
    active_locations: HashMap<String, bool>,  // Track which locations activated
    activation_prob: f64,
}
```

**SimulationBuilder Integration**:
- Calls `buggify_init()` before each iteration
- Calls `buggify_reset()` after iteration
- Tracks which buggify locations were activated across runs

### Configuration Per Test

**Standard Chaos** (moderate):
```rust
buggify_init(0.5, 0.25);  // 50% activate, 25% fire
```

**Aggressive Chaos**:
```rust
buggify_init(0.9, 0.9);  // 90% activate, 90% fire
```

**Minimal Chaos** (debugging):
```rust
buggify_init(0.1, 0.1);  // 10% activate, 10% fire
```

**Disabled**:
```rust
buggify_init(0.0, 0.0);  // No chaos
```

### Usage Example: Actor Activation Race

```rust
// In ActorCatalog::get_or_create()
if buggify_with_prob!(0.3) {
    // Inject delay before directory registration
    // Increases chance of concurrent activation race
    time_provider.sleep(Duration::from_millis(50)).await?;
}

// Register with directory
let registered_node = directory.register(actor_id, my_node_id).await?;

if buggify!() {
    // Inject delay after registration
    // Tests stale cache scenarios
    time_provider.sleep(Duration::from_millis(100)).await?;
}
```

### Alternatives Considered

**Manual random injection**: Rejected because:
- Not deterministic (can't reproduce with same seed)
- No coverage tracking
- Verbose (require RNG instance at every site)

---

## 3. Provider Pattern Research

### Decision: Generic Over Provider Traits with Clone

**Rationale**: Enables seamless simulation/production switching while maintaining single-threaded execution and deterministic testing.

### Trait Signatures

#### TimeProvider
```rust
#[async_trait(?Send)]
pub trait TimeProvider: Clone {
    async fn sleep(&self, duration: Duration) -> SimulationResult<()>;
    fn now(&self) -> Instant;
    async fn timeout<F, T>(&self, duration: Duration, future: F)
        -> SimulationResult<Result<T, ()>>
    where
        F: std::future::Future<Output = T>;
}
```

**Implementations**:
- `SimTimeProvider` - Logical time for simulation
- `TokioTimeProvider` - Wall-clock time for production

#### TaskProvider
```rust
#[async_trait(?Send)]
pub trait TaskProvider: Clone {
    fn spawn_task<F>(&self, name: &str, future: F) -> tokio::task::JoinHandle<()>
    where
        F: Future<Output = ()> + 'static;

    async fn yield_now(&self);
}
```

**Implementation**:
- `TokioTaskProvider` - Uses `tokio::task::Builder::spawn_local()`

#### NetworkProvider
```rust
#[async_trait(?Send)]
pub trait NetworkProvider: Clone {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;
    type TcpListener: TcpListenerTrait<TcpStream = Self::TcpStream> + 'static;

    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener>;
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream>;
}
```

**Implementations**:
- `SimNetworkProvider` - Simulated network with latency injection
- `TokioNetworkProvider` - Real TCP via `tokio::net`

### Obtaining Provider Instances

#### In Simulation (SimulationBuilder)
```rust
// SimulationBuilder creates SimWorld
let sim = SimWorld::new_with_network_config_and_seed(network_config, seed);

// Obtain providers from SimWorld
let network_provider = sim.network_provider();    // SimNetworkProvider
let time_provider = sim.time_provider();          // SimTimeProvider
let task_provider = sim.task_provider();          // TokioTaskProvider

// Pass to workload closure
let handle = tokio::task::spawn_local((workload.workload)(
    random_provider,
    network_provider,
    time_provider,
    task_provider,
    topology,
));
```

#### In Production (TokioRunner)
```rust
// TokioRunner creates concrete providers directly
let network_provider = TokioNetworkProvider::new();
let time_provider = TokioTimeProvider::new();
let task_provider = TokioTaskProvider;

// Pass to workload closure (same signature as simulation)
```

### Injection Pattern: Constructor Parameters

**Actor Structure**:
```rust
pub struct BankAccountActor<N, T, TP>
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
{
    network: N,
    time: T,
    task_provider: TP,
    balance: u64,
}

impl<N, T, TP> BankAccountActor<N, T, TP> {
    pub fn new(
        network: N,
        time: T,
        task_provider: TP,
    ) -> Self {
        Self {
            network,
            time,
            task_provider,
            balance: 0,
        }
    }
}
```

**Workload Function Signature**:
```rust
async fn bank_account_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,     // Injected by SimulationBuilder
    time: SimTimeProvider,            // Injected by SimulationBuilder
    task_provider: TokioTaskProvider, // Injected by SimulationBuilder
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let mut actor = BankAccountActor::new(network, time, task_provider);
    actor.run().await
}
```

### Passing Providers Through Call Stack

**Pattern 1: Store in Struct**
```rust
impl<N, T, TP> BankAccountActor<N, T, TP> {
    pub async fn deposit(&mut self, amount: u64) -> Result<()> {
        // Use stored provider
        self.time.sleep(Duration::from_millis(10)).await?;
        self.balance += amount;
        Ok(())
    }
}
```

**Pattern 2: Clone and Pass to Sub-Components**
```rust
impl<N, T, TP> BankAccountActor<N, T, TP> {
    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        // Create message bus, passing cloned providers
        let message_bus = MessageBus::new(
            self.network.clone(),
            self.time.clone(),
            self.task_provider.clone(),
        );

        // Use message bus
        message_bus.send(message).await?;
        Ok(SimulationMetrics::default())
    }
}
```

**Pattern 3: Temporary Clone for Select**
```rust
let time_provider = self.time.clone();

tokio::select! {
    result = self.process_message() => result,
    _ = time_provider.sleep(timeout) => Err(ActorError::Timeout),
}
```

### Key Design Principles

1. **Trait Bounds**: Always use trait bounds, never concrete types in APIs
2. **Clone Propagation**: All providers implement Clone for distribution
3. **Generic Actors**: Business logic generic over providers enables sim/prod switching
4. **Explicit Passing**: No globals, all providers passed explicitly
5. **Async Trait**: `#[async_trait(?Send)]` for single-threaded async traits

### Alternatives Considered

**Global provider access**: Rejected because:
- Breaks testability
- Not deterministic (hidden dependencies)
- Violates constitution principle IV (trait-based design)

**Dependency injection container**: Rejected because:
- Runtime overhead
- Requires reflection/dynamic dispatch
- Compile-time trait bounds are simpler and faster

---

## 4. Rust Async Patterns Research

### Decision: `Builder::new_current_thread().build_local()`

**Rationale**: Matches moonpool-foundation's single-threaded execution model, enables deterministic simulation.

### Proper Runtime Configuration

```rust
// Correct pattern (from foundation)
let local_runtime = tokio::runtime::Builder::new_current_thread()
    .enable_io()
    .enable_time()
    .build_local(Default::default())?;

local_runtime.block_on(async move {
    // All async code runs here
});
```

**Key Details**:
- `new_current_thread()` - Single-threaded scheduler
- `build_local()` - Returns `LocalRuntime` (no Send bounds)
- `enable_io()` + `enable_time()` - Required for networking and timers

### Task Spawning Without LocalSet

**Pattern**: Use `tokio::task::spawn_local()` directly
```rust
// Via TaskProvider (preferred)
task_provider.spawn_task("actor_message_loop", async move {
    actor.process_messages().await;
});

// Or directly (simulation only, not production)
tokio::task::spawn_local(async move {
    // Task runs on current thread
});
```

**Forbidden**: `tokio::task::LocalSet` (per constitution)

### Avoiding Accidental Send Bounds

**Always use `?Send` in async traits**:
```rust
#[async_trait(?Send)]
pub trait Actor {
    async fn on_activate(&mut self) -> Result<()>;
    async fn process_message(&mut self, msg: Message) -> Result<Response>;
}
```

**Common mistake**:
```rust
#[async_trait]  // ❌ Adds Send bound by default!
pub trait Actor {
    async fn on_activate(&mut self) -> Result<()>;
}
```

### Performance Characteristics

**Single-threaded vs Multi-threaded**:
- **Lower latency**: No cross-thread synchronization
- **Lower memory**: No Arc/Mutex overhead
- **Better cache locality**: All data on same thread
- **Deterministic**: Execution order reproducible with same seed

---

## 5. Eventual Consistency Patterns Research

### Decision: Version Vectors for Directory Convergence

**Rationale**: Provides causality tracking without central coordination, suitable for eventual consistency model.

### Cache Invalidation Strategy

**Pattern**: Piggyback invalidation on responses (Orleans model)

```rust
pub struct Message {
    // ...other fields...
    cache_invalidation: Option<Vec<CacheUpdate>>,
}

pub struct CacheUpdate {
    invalid_address: ActorAddress,
    valid_address: Option<ActorAddress>,
}
```

**Flow**:
1. Node A caches: `actor_123 → node_2`
2. Actor deactivates on node_2, activates on node_3
3. Node A sends message to node_2 (stale cache)
4. Node_2 responds with cache invalidation: `invalid=node_2, valid=node_3`
5. Node A updates cache: `actor_123 → node_3`
6. Node A retries message to node_3

### Read-Your-Writes Consistency

**Pattern**: Local directory write-through

```rust
impl Directory {
    pub async fn register(&mut self, actor_id: ActorId, node_id: NodeId) -> Result<()> {
        // Update local cache immediately
        self.local_cache.insert(actor_id.clone(), node_id);

        // Propagate to other nodes (async, no wait)
        self.broadcast_update(actor_id, node_id).await;

        Ok(())
    }

    pub fn lookup(&self, actor_id: &ActorId) -> Option<NodeId> {
        // Read from local cache (includes own writes)
        self.local_cache.get(actor_id).copied()
    }
}
```

### Convergence Testing Strategy

**Invariant**: Eventually all nodes agree on actor locations

```rust
// In test assertions
let locations = Vec::new();
for node in &nodes {
    locations.push(node.directory.lookup(&actor_id));
}

// Eventually converge (allow time for propagation)
eventually_assert!(
    locations.iter().all(|loc| loc == &locations[0]),
    "All nodes converged to same location for actor"
);
```

---

## 6. State Machine Patterns Research

### Decision: Type-State Pattern for Actor Lifecycle

**Rationale**: Compile-time guarantees for state transitions, impossible to call methods in wrong state.

### Enum-Based State Machine

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationState {
    Creating,     // Grain instance created, not yet activated
    Activating,   // OnActivateAsync() in progress
    Valid,        // Ready to process messages
    Deactivating, // OnDeactivateAsync() in progress
    Invalid,      // Disposed, no longer usable
}
```

**Transition Validation**:
```rust
impl ActivationState {
    pub fn can_transition_to(&self, next: ActivationState) -> bool {
        use ActivationState::*;
        matches!(
            (self, next),
            (Creating, Activating) |
            (Activating, Valid) |
            (Activating, Deactivating) |  // Activation failed
            (Valid, Deactivating) |
            (Deactivating, Invalid)
        )
    }
}
```

**Guarded Transitions**:
```rust
impl ActorContext {
    fn set_state(&mut self, next: ActivationState) -> Result<()> {
        if !self.state.can_transition_to(next) {
            return Err(ActorError::InvalidStateTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        Ok(())
    }
}
```

### State-Specific Data via Enum Variants

**Advanced Pattern** (future consideration):
```rust
pub enum ActorState {
    Creating { timestamp: Instant },
    Activating { started_at: Instant },
    Valid { message_queue: VecDeque<Message> },
    Deactivating { reason: DeactivationReason },
    Invalid,
}
```

### Testing State Machine Exhaustiveness

```rust
#[test]
fn test_all_transitions() {
    use ActivationState::*;

    let states = [Creating, Activating, Valid, Deactivating, Invalid];

    for &from in &states {
        for &to in &states {
            let allowed = from.can_transition_to(to);
            // Verify expected transitions
        }
    }
}
```

---

## Summary: Research Decisions

| Area | Decision | Key Insight |
|------|----------|-------------|
| **Transport** | Peer + custom ActorEnvelope | ActorId addressing requires custom protocol |
| **Serialization** | serde_json for payloads | Human-readable, debuggable, zero boilerplate |
| **Chaos** | buggify!() with 0.5/0.25 defaults | Deterministic, location-based, tracked coverage |
| **Providers** | Generic with Clone + trait bounds | Seamless sim/prod switching, compile-time checks |
| **Runtime** | new_current_thread + build_local | Single-threaded, deterministic, no LocalSet |
| **Consistency** | Eventual with cache invalidation | Matches Orleans, suitable for virtual actors |
| **State** | Explicit enum with guarded transitions | Type-safe, compile-time validation |

---

## Implementation Guidance

### Phase 1: Core Message Types
1. Define `ActorId`, `NodeId`, `CorrelationId` types
2. Implement `ActorEnvelope` with custom wire format
3. Implement `ActorEnvelopeSerializer` trait
4. Unit tests for serialization round-trip

### Phase 2: Actor Lifecycle
1. Define `ActivationState` enum
2. Implement `ActorContext` with state machine
3. Define `Actor` trait with lifecycle hooks
4. Unit tests for state transitions

### Phase 3: Message Bus
1. Implement `MessageBus` generic over providers
2. Implement correlation tracking with `CallbackData`
3. Implement timeout enforcement
4. Integration tests with buggify injection

### Phase 4: Directory
1. Implement `Directory` trait
2. Implement `SimpleDirectory` with eventual consistency
3. Implement two-random-choices placement
4. Multi-node tests with cache invalidation

### Phase 5: End-to-End
1. Implement `ActorCatalog` with double-check locking
2. Implement `ActorSystem` entry point
3. BankAccount example actor
4. Multi-topology simulation tests (1x1, 2x2, 10x10)

---

**Files Referenced**:
- PeerTransport: `moonpool-foundation/src/network/peer/core.rs`
- Buggify: `moonpool-foundation/src/buggify.rs`
- Providers: `moonpool-foundation/src/time/provider.rs`, `moonpool-foundation/src/task/mod.rs`, `moonpool-foundation/src/network/traits.rs`
- Tests: `moonpool-foundation/tests/simulation/ping_pong/tests.rs`
