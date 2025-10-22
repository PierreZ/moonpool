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

## 7. Concurrency Patterns Research

### Decision: Double-Check Locking for Activation Creation

**Rationale**: Prevents duplicate activations while minimizing lock contention (Orleans pattern from `Catalog.cs:106-195`).

### Pattern: Get-or-Create with Race Protection

```rust
pub struct ActorCatalog<A: Actor> {
    activation_directory: ActivationDirectory<A>,
    activation_lock: RefCell<()>,  // Coarse lock for get-or-create
    node_id: NodeId,
    directory: Arc<dyn Directory>,
}

impl<A: Actor> ActorCatalog<A> {
    pub fn get_or_create_activation(&self, actor_id: ActorId) -> Result<Arc<ActorContext<A>>> {
        // FAST PATH: Check without lock (99% case - activation exists)
        if let Some(activation) = self.activation_directory.find_activation(&actor_id) {
            return Ok(activation);
        }

        // SLOW PATH: Acquire lock (only on first access per actor)
        let _guard = self.activation_lock.borrow_mut();

        // DOUBLE-CHECK under lock (protect against TOCTOU race)
        if let Some(activation) = self.activation_directory.find_activation(&actor_id) {
            return Ok(activation);  // Another task created while we waited
        }

        // Create new activation (while holding lock)
        let context = Arc::new(ActorContext::new(actor_id.clone(), self.node_id.clone()));
        self.activation_directory.record_activation(context.clone());

        // Return for activation outside lock
        Ok(context)
    }
}
```

### Why This Pattern?

1. **Fast Path Optimization** (99% case)
   - Activation already exists → no lock needed
   - Read-only operation on HashMap via RefCell
   - Minimal overhead (~10ns)

2. **Race-Safe** (1% case - first access)
   - Coarse lock serializes all activation creation
   - Double-check prevents TOCTOU (Time-Of-Check-Time-Of-Use) races
   - Two tasks requesting same actor: first creates, second uses

3. **Deadlock-Free**
   - Activation happens **outside** lock (after returning from get_or_create)
   - Lock only held during HashMap operations
   - No nested lock acquisitions

4. **Single-Threaded Adaptation** (Moonpool-specific)
   - Orleans uses `lock(activations)` (multi-threaded)
   - Moonpool uses `RefCell` (single-threaded, no Send/Sync)
   - Same semantics, lower overhead

### Race Scenario Example

```rust
// Timeline: Two nodes receive message for "alice" simultaneously

// Node 1 Thread              | Node 2 Thread
// ---------------------------|---------------------------
// get_or_create("alice")     | get_or_create("alice")
// → Fast check: None         | → Fast check: None
// → Acquire lock             | → Wait for lock...
// → Double-check: None       |
// → Create activation        |
// → Record in directory      |
// → Release lock             | → Acquire lock
// → Return context           | → Double-check: Some!
//                            | → Release lock
//                            | → Return existing context
```

**Result**: Node 1 creates activation, Node 2 reuses it. No duplicates!

### Activation Outside Lock (Critical Detail)

```rust
// CALLER of get_or_create
let context = catalog.get_or_create_activation(actor_id)?;

// Activate AFTER releasing lock (avoid I/O while holding lock)
context.activate(message).await?;
```

**Why activate outside lock**:
- `on_activate()` may perform I/O (load state, connect to DB)
- Holding lock during I/O blocks all other activation creation
- Lock only protects HashMap mutation, not actor initialization

### Testing Strategy

**Buggify injection points**:
```rust
pub fn get_or_create_activation(&self, actor_id: ActorId) -> Result<Arc<ActorContext<A>>> {
    if let Some(activation) = self.activation_directory.find_activation(&actor_id) {
        return Ok(activation);
    }

    // BUGGIFY: Inject delay before acquiring lock (increase race probability)
    if buggify_with_prob!(0.5) {
        time_provider.sleep(Duration::from_millis(10)).await?;
    }

    let _guard = self.activation_lock.borrow_mut();

    // BUGGIFY: Inject delay after lock (test double-check effectiveness)
    if buggify_with_prob!(0.3) {
        time_provider.sleep(Duration::from_millis(5)).await?;
    }

    if let Some(activation) = self.activation_directory.find_activation(&actor_id) {
        return Ok(activation);
    }

    let context = Arc::new(ActorContext::new(actor_id.clone(), self.node_id.clone()));
    self.activation_directory.record_activation(context.clone());
    Ok(context)
}
```

**Invariant to test**:
```rust
always_assert!(
    activation_directory.count() == unique_actor_ids.len(),
    "No duplicate activations created"
);
```

### Alternatives Considered

**Fine-grained locking** (per-actor locks):
- Rejected: Complex, requires lock management
- Orleans uses coarse lock - simple and sufficient

**Lock-free data structures**:
- Rejected: Not needed in single-threaded runtime
- RefCell provides same guarantees with less complexity

---

## 8. Storage Provider Research

### Decision: Typed State with ActorState<T> Wrapper

**UPDATE (2025-10-22)**: The final design uses `ActorState<T>` wrapper for type-safe persistence. See data-model.md for complete implementation. The analysis below documents the original research that led to this design.

**Rationale**: Simple load/save interface without concurrency control, relying on single-activation guarantee. Framework manages serialization through ActorState<T> wrapper. Matches Orleans philosophy but intentionally simpler (no ETags, transactions, or automatic persistence).

### Storage Provider Trait

**Pattern**: Async trait with Result-based error handling

```rust
#[async_trait(?Send)]
pub trait StorageProvider: Clone {
    /// Load state for an actor. Returns None if no state exists.
    ///
    /// **Key Format**: Includes namespace/actor_type::key for natural isolation
    /// **Example**: "prod::BankAccount/alice" → Some(Vec<u8>)
    async fn load_state(&self, actor_id: &ActorId) -> Result<Option<Vec<u8>>, StorageError>;

    /// Save state for an actor.
    ///
    /// **Overwrites**: Existing state is completely replaced (no merging)
    /// **Atomicity**: Implementation-defined (naive storage may not be atomic)
    async fn save_state(&self, actor_id: &ActorId, data: Vec<u8>) -> Result<(), StorageError>;
}
```

**Key Design Choices**:
- **No clear_state()**: Actors save empty state or delete via implementation-specific means
- **No ETags**: No optimistic concurrency control (assumes single activation per actor)
- **No transactions**: Each operation independent (no multi-key atomic updates)
- **Opaque data**: Storage sees Vec<u8>, actor handles serialization

### Error Types

```rust
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Storage unavailable")]
    Unavailable,

    #[error("Key not found: {0}")]
    NotFound(String),

    #[error("Storage operation failed: {0}")]
    OperationFailed(String),
}
```

**Error Handling Philosophy**:
- **No automatic retry**: Caller decides retry strategy
- **Explicit propagation**: Actor methods return Result, caller handles errors
- **Fail-visible**: Storage failures surface to actor logic, not hidden

### Integration with Actor Lifecycle

**Pattern**: Load on activation, explicit save during message processing

```rust
#[async_trait(?Send)]
pub trait Actor {
    async fn on_activate(
        &mut self,
        storage: Option<&dyn StorageProvider>,
    ) -> Result<(), ActorError> {
        // Actor decides whether to load state
        if let Some(storage) = storage {
            if let Some(data) = storage.load_state(&self.actor_id()).await? {
                self.deserialize_state(&data)?;
            }
        }
        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError>;
}
```

**Actor Method with Save**:
```rust
impl BankAccountActor {
    pub async fn deposit(
        &mut self,
        amount: u64,
        storage: &dyn StorageProvider,
    ) -> Result<u64, ActorError> {
        // Update in-memory state
        self.balance += amount;

        // Explicitly save to storage
        let state_data = serde_json::to_vec(&self.balance)?;
        storage.save_state(&self.actor_id, state_data).await
            .map_err(|e| ActorError::StorageFailed(e))?;

        Ok(self.balance)
    }
}
```

### InMemoryStorage Implementation

**For Testing** (deterministic, fast):

```rust
#[derive(Clone)]
pub struct InMemoryStorage {
    data: Rc<RefCell<HashMap<String, Vec<u8>>>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self {
            data: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    fn storage_key(actor_id: &ActorId) -> String {
        // Format: "namespace::actor_type/key"
        format!("{}::{}/{}", actor_id.namespace, actor_id.actor_type, actor_id.key)
    }
}

#[async_trait(?Send)]
impl StorageProvider for InMemoryStorage {
    async fn load_state(&self, actor_id: &ActorId) -> Result<Option<Vec<u8>>, StorageError> {
        let key = Self::storage_key(actor_id);
        Ok(self.data.borrow().get(&key).cloned())
    }

    async fn save_state(&self, actor_id: &ActorId, data: Vec<u8>) -> Result<(), StorageError> {
        let key = Self::storage_key(actor_id);
        self.data.borrow_mut().insert(key, data);
        Ok(())
    }
}
```

**Simulation Support** (for Buggify):
```rust
#[async_trait(?Send)]
impl StorageProvider for InMemoryStorage {
    async fn save_state(&self, actor_id: &ActorId, data: Vec<u8>) -> Result<(), StorageError> {
        // BUGGIFY: Inject random save failures
        if buggify_with_prob!(0.1) {
            return Err(StorageError::OperationFailed("Simulated storage failure".to_string()));
        }

        let key = Self::storage_key(actor_id);
        self.data.borrow_mut().insert(key, data);
        Ok(())
    }
}
```

### File-Based Storage (Optional)

**For Persistent Testing**:

```rust
#[derive(Clone)]
pub struct FileStorage {
    base_path: PathBuf,
}

impl FileStorage {
    fn file_path(&self, actor_id: &ActorId) -> PathBuf {
        // Create directory per namespace/actor_type
        self.base_path
            .join(&actor_id.namespace)
            .join(&actor_id.actor_type)
            .join(format!("{}.json", actor_id.key))
    }
}

#[async_trait(?Send)]
impl StorageProvider for FileStorage {
    async fn load_state(&self, actor_id: &ActorId) -> Result<Option<Vec<u8>>, StorageError> {
        let path = self.file_path(actor_id);

        if !path.exists() {
            return Ok(None);
        }

        let data = tokio::fs::read(&path).await?;
        Ok(Some(data))
    }

    async fn save_state(&self, actor_id: &ActorId, data: Vec<u8>) -> Result<(), StorageError> {
        let path = self.file_path(actor_id);

        // Create parent directories
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Write atomically (temp file + rename)
        let temp_path = path.with_extension("tmp");
        tokio::fs::write(&temp_path, data).await?;
        tokio::fs::rename(&temp_path, &path).await?;

        Ok(())
    }
}
```

### Key Isolation via ActorId Structure

**Storage Key Format**: `namespace::actor_type/key`

**Examples**:
- `"prod::BankAccount/alice"` → Different from `"staging::BankAccount/alice"`
- `"tenant-acme::User/bob"` → Isolated from `"tenant-globex::User/bob"`
- `"dev::Session/session-123"` → Different type than `"dev::User/session-123"`

**Benefits**:
- Natural multi-tenancy (namespace isolation)
- Type safety (different actor types use different directories/tables)
- Debuggability (human-readable keys in logs)
- Cluster boundary enforcement (cannot access cross-namespace state)

### Comparison with Orleans

| Feature | Orleans | Moonpool (This Phase) |
|---------|---------|----------------------|
| **Interface** | IPersistentState<T> | StorageProvider trait |
| **Operations** | Read, Write, Clear | load_state, save_state |
| **Concurrency** | ETags (optimistic locking) | None (single activation) |
| **Lifecycle** | Automatic load on activation | Explicit load in on_activate() |
| **Persistence** | Automatic via attribute | Explicit save_state() calls |
| **Transactions** | Support via providers | Not supported (future) |
| **Migration** | Dehydrate/rehydrate | Not supported (future) |
| **Multiple states** | Multiple [PersistentState] | Multiple storage keys per actor |

**Key Simplifications**:
1. No automatic loading (actor calls load explicitly in on_activate)
2. No automatic saving (actor calls save_state after mutations)
3. No ETags (trust single activation guarantee)
4. No migration support (state not transferred during deactivation)
5. No transactions (each save independent)

**Rationale for Simplifications**:
- Focus on core distributed pattern first
- Single activation eliminates concurrency races
- Explicit operations make control flow clear
- Transactions/migration can be added later without breaking changes

### Testing Strategy

**Unit Tests** (storage implementations):
```rust
#[tokio::test]
async fn test_save_and_load() {
    let storage = InMemoryStorage::new();
    let actor_id = ActorId::new("test", "BankAccount", "alice");

    // Save state
    let state = serde_json::to_vec(&100u64).unwrap();
    storage.save_state(&actor_id, state.clone()).await.unwrap();

    // Load state
    let loaded = storage.load_state(&actor_id).await.unwrap();
    assert_eq!(loaded, Some(state));
}

#[tokio::test]
async fn test_namespace_isolation() {
    let storage = InMemoryStorage::new();

    // Save to prod namespace
    let prod_actor = ActorId::new("prod", "BankAccount", "alice");
    storage.save_state(&prod_actor, vec![1, 2, 3]).await.unwrap();

    // Load from staging namespace (different actor)
    let staging_actor = ActorId::new("staging", "BankAccount", "alice");
    let loaded = storage.load_state(&staging_actor).await.unwrap();
    assert_eq!(loaded, None); // No cross-namespace access
}
```

**Simulation Tests** (with Buggify):
```rust
async fn storage_chaos_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    // Use InMemoryStorage with Buggify injection
    let storage = InMemoryStorage::new();

    // Create actor runtime with storage
    let runtime = ActorRuntime::with_storage(
        "test",
        network,
        time,
        task_provider,
        storage,
    ).await?;

    // Perform operations that save state
    let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice");

    // Some saves will fail due to buggify
    for i in 0..100 {
        match alice.call(DepositRequest { amount: 100 }).await {
            Ok(_) => { /* Success */ }
            Err(ActorError::StorageFailed(_)) => {
                // Expected under chaos - retry or handle
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(SimulationMetrics::default())
}
```

**Invariant: State Consistency** (if actor saves state):
```rust
// After deactivation and reactivation, state should match
let balance_before = alice.call(GetBalanceRequest).await?;
runtime.deactivate_actor(&alice.actor_id()).await?;

// Reactivate by sending new message
let balance_after = alice.call(GetBalanceRequest).await?;

always_assert!(
    balance_before == balance_after,
    "State persisted correctly across deactivation"
);
```

### Alternatives Considered

**Automatic persistence** (Orleans model):
- Rejected: Adds complexity (when to save? on every mutation?)
- Explicit save gives actors control over persistence timing
- Reduces storage I/O (actor can batch mutations)

**Optimistic locking with ETags**:
- Rejected: Single activation guarantee eliminates concurrent writers
- No need for conflict detection if only one instance active
- Can be added later if multi-activation support needed

**Transactional storage**:
- Rejected: Out of scope for naive storage phase
- Adds complexity (rollback, commit, isolation levels)
- Focus on core distributed pattern first

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
| **Storage** | Trait with load/save only | Naive shared storage, no concurrency control |

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
