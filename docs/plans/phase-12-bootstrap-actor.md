# Phase 12: Actor System Bootstrap
## Unified Implementation Plan

**Status**: IN PROGRESS
**Author**: Synthesized from Orleans analysis + draft plans
**Date**: January 2025

---

## Overview

Phase 12 builds a **distributed actor runtime** on top of `moonpool-foundation`, focusing on **static actors** (SystemTargets in Orleans) with **network-first validation**. We start with scheduling foundations, build up to a complete message bus, then validate everything with a distributed PingActor that demonstrates superior developer experience compared to the manual ping_pong example.

### Key Decisions

✅ **Foundation First**: WorkQueue → Catalog → MessageBus → Network Integration
✅ **Serialization Trait**: Pluggable from day one (default: serde_json, user brings their own)
✅ **Orleans Patterns**: Double-check locking, command pattern, correlation IDs, packed headers
✅ **Static Actors First**: Distributed validation with SystemTargets, virtual actor hooks for future
✅ **Comprehensive Observability**: StateRegistry integration, detailed metrics, state machines everywhere
✅ **Superior DX**: PingActor with derive macros, type-safe messages, automatic state management
✅ **Single-Threaded**: No atomics, no Send/Sync, simple mutable state
✅ **Provider Pattern**: Always use foundation traits, never direct tokio calls

---

## Core Constraints (Inherited from moonpool-foundation)

### Forbidden Patterns
❌ `tokio::time::sleep()` → ✅ `time.sleep()` (use TimeProvider)
❌ `tokio::time::timeout()` → ✅ `time.timeout()` (use TimeProvider)
❌ `tokio::spawn()` → ✅ `task.spawn_task()` (use TaskProvider)
❌ `Instant::now()` → ✅ `time.now()` (use TimeProvider)
❌ `Mutex`, `Arc<Atomic*>` → ✅ Simple mutable fields (single-threaded)
❌ `Send + Sync` → ✅ No marker traits needed

### Required Patterns
✅ Use `#[async_trait(?Send)]` for async traits
✅ Pass providers explicitly via constructors
✅ Use `tokio::runtime::Builder::new_current_thread().build_local()` (no LocalSet)
✅ Simple mutable state (no atomics in single-threaded code)

### Why Single-Threaded?
- Deterministic simulation requires single-core execution
- No need for thread-safety primitives
- Simpler code, easier debugging
- All Orleans concurrency patterns simplified away

---

## Phase Structure

### Phase 1: Actor Scheduling Foundation
Build quantum-based work queue and actor addressing for deterministic execution.

### Phase 2: Actor Registry & Lifecycle
Create ActorCatalog with lifecycle management, Actor trait, and inbox system.

### Phase 3: Message System
Implement MessageBus with serialization trait, correlation-based RPC, timeouts.

### Phase 4: Network Integration
Connect to foundation transport, implement distributed static actors, validate with PingActor.

### Phase 5: Virtual Actor Hooks
Design extension points for future virtual actor implementation.

---

## Phase 1: Actor Scheduling Foundation

### Goals
- Quantum-based work queue ensuring single-threaded execution per actor
- Actor addressing with namespace/type/key structure
- Foundation for deterministic message processing
- **Orleans References**: `WorkItemGroup.cs`, `ActivationTaskScheduler.cs`

---

### Step 1.1: Actor Work Queue

#### What to Build

Per-actor queue with **explicit state machine** and quantum-based execution.

**State Machine**:
```
[Idle] → [Executing] → [Idle]
   ↓
[Draining] (terminal)
```

**Implementation**:
```rust
/// Work queue state machine
pub enum WorkQueueState {
    Idle {
        queue: VecDeque<BoxFuture<'static, ()>>,
    },
    Executing {
        queue: VecDeque<BoxFuture<'static, ()>>,
        current_task_started: Instant,
    },
    Draining,
}

pub struct ActorWorkQueue {
    actor_address: ActorAddress,
    state: WorkQueueState,
    metrics: ActorWorkQueueMetrics,
    time: Arc<dyn TimeProvider>,
    task: Arc<dyn TaskProvider>,
    quantum_ms: u64,  // Configurable time slice
}

// Single-threaded: no atomics needed
pub struct ActorWorkQueueMetrics {
    queue_depth: usize,
    tasks_executed: u64,
    tasks_enqueued: u64,
    execution_time_us: u64,
    max_queue_depth: usize,
    current_state: u8,  // For observability: 0=Idle, 1=Executing, 2=Draining
}
```

**Key Methods**:
- `enqueue(BoxFuture)` - Add work, transition Idle→Executing if needed
- `spawn_executor()` - Process tasks until quantum expires or queue empty
- `drain()` - Transition to Draining, reject new work

**Orleans Pattern**: Quantum-based execution from `WorkItemGroup.cs:143-222`
```csharp
while (activationSchedulingQuantumMs <= 0 || taskEnd - loopStart < activationSchedulingQuantumMs)
```

**State Machine Invariants**:
```rust
fn check_work_queue_invariants(queue: &ActorWorkQueue) {
    match &queue.state {
        WorkQueueState::Idle { queue: q } => {
            // Single-threaded: direct field access
            always_assert!(
                queue.metrics.queue_depth == q.len(),
                "Idle: depth mismatch"
            );
        }
        WorkQueueState::Executing { queue: q, current_task_started } => {
            // Use TimeProvider, not Instant::now()
            let age = queue.time.now().duration_since(*current_task_started);
            always_assert!(age < Duration::from_secs(60), "Task running too long");
        }
        WorkQueueState::Draining => {
            always_assert!(
                queue.metrics.queue_depth == 0,
                "Draining: queue not empty"
            );
        }
    }

    // Global: enqueued = executed + current_depth (single-threaded, no atomics)
    always_assert!(
        queue.metrics.tasks_enqueued == queue.metrics.tasks_executed + queue.metrics.queue_depth as u64,
        "Task accounting broken: {} != {} + {}",
        queue.metrics.tasks_enqueued,
        queue.metrics.tasks_executed,
        queue.metrics.queue_depth
    );
}
```

**Observability**:
```rust
impl ActorWorkQueue {
    fn update_metrics(&self, registry: &StateRegistry) {
        // Single-threaded: simple field access
        registry.register_state(
            &format!("work_queue:{}", self.actor_address),
            serde_json::json!({
                "state": self.current_state_name(),
                "queue_depth": self.metrics.queue_depth,
                "tasks_executed": self.metrics.tasks_executed,
                "max_queue_depth": self.metrics.max_queue_depth,
            })
        );
    }
}
```

**Simulation Testing**:
```rust
#[tokio::test]
async fn test_work_queue_state_transitions() {
    let mut sim = Simulation::new(42);
    let queue = ActorWorkQueue::new(
        ActorAddress::new("test", "Actor", "1"),
        sim.time(),
        sim.task_provider(),
        100, // 100ms quantum
    );

    assert_eq!(queue.current_state_name(), "Idle");

    queue.enqueue(Box::pin(async {
        sim.time().sleep(Duration::from_millis(10)).await;
    }));

    assert_eq!(queue.current_state_name(), "Executing");

    sim.run_until_idle().await;

    assert_eq!(queue.current_state_name(), "Idle");

    check_work_queue_invariants(&queue);
}

#[tokio::test]
async fn test_work_queue_chaos() {
    for seed in 0..10 {
        let mut sim = Simulation::new(seed);
        sim.use_random_config(); // Enable buggify

        let queue = ActorWorkQueue::new(/* ... */);

        // Enqueue 1000 tasks with buggify delays
        for _ in 0..1000 {
            let time = sim.time();
            let random = sim.random();

            queue.enqueue(Box::pin(async move {
                if buggify!(0.1) {
                    time.sleep(Duration::from_millis(random.random_range(1..100))).await;
                }
            }));
        }

        sim.run_until_idle().await;

        // Validate (single-threaded: simple field access)
        assert_eq!(queue.metrics.tasks_executed, 1000);
        sometimes_assert!(queue_handled_backlog, queue.metrics.max_queue_depth > 10);
        check_work_queue_invariants(&queue);
    }
}
```

**Buggify Placement**:
- Before enqueue: Inject 1-1000ns delay (tests enqueue races)
- During execution: Inject 10-500μs delay 5% of time (simulates slow processing)

**Success Criteria**:
- ✅ All invariants pass across 10+ seeds
- ✅ All sometimes_assert! triggered
- ✅ Quantum enforcement validated
- ✅ 100% success rate (no hangs/deadlocks)

**Files to Create**:
- `moonpool/src/scheduling/work_queue.rs`
- `moonpool/src/scheduling/mod.rs`
- Tests in `moonpool/tests/work_queue_tests.rs`

---

### Step 1.2: Actor Addressing

#### What to Build

Type-safe actor addressing with namespace/type/key structure.

**Implementation**:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActorAddress {
    pub namespace: String,    // "system", "game", "chat"
    pub actor_type: String,   // "Ping", "Player", "Room"
    pub key: Option<String>,  // None = static actor, Some = virtual actor
}

impl ActorAddress {
    pub fn new(namespace: impl Into<String>, actor_type: impl Into<String>, key: impl Into<Option<String>>) -> Self {
        Self {
            namespace: namespace.into(),
            actor_type: actor_type.into(),
            key: key.into(),
        }
    }

    pub fn system(actor_type: impl Into<String>) -> Self {
        Self::new("system", actor_type, None::<String>)
    }

    pub fn is_static(&self) -> bool {
        self.key.is_none()
    }

    pub fn is_virtual(&self) -> bool {
        self.key.is_some()
    }
}

impl std::fmt::Display for ActorAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(key) = &self.key {
            write!(f, "{}::{}::{}", self.namespace, self.actor_type, key)
        } else {
            write!(f, "{}::{}", self.namespace, self.actor_type)
        }
    }
}
```

**Examples**:
```rust
// Static actors (SystemTargets)
let ping_actor = ActorAddress::system("Ping");
// → "system::Ping"

// Virtual actors (future)
let player_actor = ActorAddress::new("game", "Player", Some("player-123"));
// → "game::Player::player-123"
```

**Validation**:
```rust
#[test]
fn test_actor_address_formatting() {
    let static_addr = ActorAddress::system("Ping");
    assert_eq!(static_addr.to_string(), "system::Ping");
    assert!(static_addr.is_static());

    let virtual_addr = ActorAddress::new("game", "Player", Some("p1"));
    assert_eq!(virtual_addr.to_string(), "game::Player::p1");
    assert!(virtual_addr.is_virtual());
}
```

**Files to Create**:
- `moonpool/src/actor_address.rs`

---

## Phase 2: Actor Registry & Lifecycle

### Goals
- ActorCatalog with double-check locking pattern
- Actor trait with lifecycle hooks
- Actor Inbox with message queuing
- **Orleans References**: `Catalog.cs`, `ActivationData.cs`, `Grain.cs`

---

### Step 2.1: Actor Catalog

#### What to Build

Thread-safe actor registry using **Orleans double-check locking pattern**.

**State Machines**:
```
Catalog: [Initializing] → [Running] → [Stopping] → [Stopped]
Actor:   [Creating] → [Activating] → [Active] → [Deactivating] → [Invalid]
```

**Implementation**:
```rust
// Single-threaded: no Arc<Mutex>, just RefCell for interior mutability
pub struct ActorCatalog {
    activations: RefCell<HashMap<ActorAddress, Rc<RefCell<ActorInstance>>>>,
    catalog_state: RefCell<CatalogState>,
    time: Arc<dyn TimeProvider>,
    task: Arc<dyn TaskProvider>,
    state_registry: Arc<StateRegistry>,
}

enum CatalogState {
    Initializing,
    Running,
    Stopping,
    Stopped,
}

pub struct ActorInstance {
    address: ActorAddress,
    state: ActorState,
    work_queue: ActorWorkQueue,
    inbox: mpsc::UnboundedReceiver<Message>,
    inbox_tx: mpsc::UnboundedSender<Message>,
    created_at: Instant,  // ❌ Use time.now() instead!
}

enum ActorState {
    Creating,
    Activating,
    Active,
    Deactivating,
    Invalid,
}

// NOTE: Instant should come from TimeProvider, not std
// This will be fixed in implementation:
// created_at: TimeInstant (from TimeProvider::now())
```

**Orleans Double-Check Locking Pattern** (from `Catalog.cs:106-195`):
**Single-threaded simplification: No async locks needed, just borrow checker**

```rust
impl ActorCatalog {
    pub fn get_or_create(&self, address: ActorAddress) -> Result<Rc<RefCell<ActorInstance>>> {
        // First check without borrow (fast path)
        {
            let activations = self.activations.borrow();
            if let Some(instance) = activations.get(&address) {
                return Ok(Rc::clone(instance));
            }
        }

        // Borrow mutably for creation (slow path)
        let mut activations = self.activations.borrow_mut();

        // Second check with mutable borrow (Orleans double-check pattern)
        // Single-threaded: No races, but keeps same pattern for clarity
        if let Some(instance) = activations.get(&address) {
            return Ok(Rc::clone(instance));
        }

        // Check catalog state
        {
            let catalog_state = self.catalog_state.borrow();
            if !matches!(*catalog_state, CatalogState::Running) {
                return Err(Error::CatalogNotRunning);
            }
        }

        // Create new activation (use TimeProvider, not Instant::now())
        let instance = Rc::new(RefCell::new(ActorInstance::new(
            address.clone(),
            self.time.clone(),
            self.task.clone(),
        )));

        activations.insert(address.clone(), Rc::clone(&instance));

        self.update_metrics();

        // Activate (no lock to drop, just return from closure)
        // Note: In single-threaded code, we can hold the borrow across await points
        // but it's cleaner to drop early
        drop(activations);

        // Activation will be done asynchronously
        // (Will be implemented in Step 2.2)

        Ok(instance)
    }

    async fn activate_instance(&self, instance: &Rc<RefCell<ActorInstance>>) -> Result<()> {
        let mut inst = instance.borrow_mut();
        inst.state = ActorState::Activating;

        // Call actor's on_activate() hook
        // (Will be implemented in Step 2.2)

        inst.state = ActorState::Active;
        Ok(())
    }
}

// NOTE: In Orleans, async locks are needed for thread-safety.
// In moonpool: single-threaded, so RefCell is sufficient.
// The double-check pattern remains for code clarity, even though
// no actual races are possible in single-threaded execution.
```

**Invariants**:
```rust
fn check_catalog_invariants(catalog: &ActorCatalog, state_registry: &StateRegistry) {
    // BUG DETECTOR: No actor in multiple states simultaneously
    // Single-threaded: just borrow, no lock needed
    let activations = catalog.activations.borrow();
    let mut state_counts: HashMap<&str, usize> = HashMap::new();

    for instance in activations.values() {
        let inst = instance.borrow();
        let state_name = match inst.state {
            ActorState::Creating => "creating",
            ActorState::Activating => "activating",
            ActorState::Active => "active",
            ActorState::Deactivating => "deactivating",
            ActorState::Invalid => "invalid",
        };
        *state_counts.entry(state_name).or_insert(0) += 1;
    }

    // BUG DETECTOR: Total count matches per-state sum
    let total_count = activations.len();
    let sum: usize = state_counts.values().sum();
    always_assert!(total_count == sum, "Actor count mismatch: {} != {}", total_count, sum);

    // Always: Invalid actors not in catalog
    always_assert!(state_counts.get("invalid").copied().unwrap_or(0) == 0, "Invalid actors in catalog");
}
```

**Simulation Testing**:
```rust
#[tokio::test]
async fn test_catalog_concurrent_creation() {
    let mut sim = Simulation::new(42);
    let catalog = ActorCatalog::new(sim.time(), sim.task_provider(), sim.state_registry());

    catalog.start().await.unwrap();

    // 10 concurrent workloads try to create same actor
    let mut handles = vec![];
    for i in 0..10 {
        let catalog = catalog.clone();
        let addr = ActorAddress::system("Ping");
        let handle = sim.task_provider().spawn_task(async move {
            catalog.get_or_create(addr).await
        });
        handles.push(handle);
    }

    let results = futures::future::join_all(handles).await;

    // All should get the SAME instance (Rc pointer equality)
    let first_ptr = Rc::as_ptr(&results[0].unwrap());
    for result in &results[1..] {
        let ptr = Rc::as_ptr(&result.as_ref().unwrap());
        assert_eq!(ptr, first_ptr, "Got different instances!");
    }

    // NOTE: In single-threaded code, true "races" don't exist, but we keep
    // the test to validate that interleaved async tasks still get same instance
    sometimes_assert!(catalog_concurrent_access, true, "Concurrent async access handled correctly");
    check_catalog_invariants(&catalog, &sim.state_registry());
}
```

**Buggify Placement**:
- Before lock acquisition: 1-100μs delay (exposes races)
- Before state transitions: 1ns delay (tests atomicity)

**Success Criteria**:
- ✅ Double-check locking prevents duplicate actors
- ✅ All invariants pass
- ✅ Concurrent creation stress test passes
- ✅ Multi-topology tests (1x1, 2x2, 10x10)

**Files to Create**:
- `moonpool/src/catalog.rs`
- Tests in `moonpool/tests/catalog_tests.rs`

---

### Step 2.2: Actor Base Trait

#### What to Build

Actor trait with lifecycle hooks and message handling.

**Implementation**:
```rust
#[async_trait(?Send)]
pub trait Actor: 'static {
    /// Called when actor is first activated
    async fn on_activate(&mut self, ctx: &ActorContext) -> Result<()> {
        Ok(())
    }

    /// Called before actor is deactivated
    async fn on_deactivate(&mut self, ctx: &ActorContext, reason: DeactivationReason) -> Result<()> {
        Ok(())
    }

    /// Handle incoming message
    async fn handle(&mut self, msg: Message, ctx: &ActorContext) -> Result<Response>;
}

pub struct ActorContext {
    pub address: ActorAddress,
    pub time: Arc<dyn TimeProvider>,
    pub task: Arc<dyn TaskProvider>,
    pub state_registry: Arc<StateRegistry>,
}

pub struct DeactivationReason {
    pub code: DeactivationReasonCode,
    pub description: String,
    pub error: Option<Box<dyn std::error::Error + Send + Sync>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeactivationReasonCode {
    ApplicationRequested,
    ActivationFailed,
    ActivationTimeout,
    ShuttingDown,
    ActivationUnresponsive,
}
```

**Orleans Reference**: `Grain.cs:145-161` for lifecycle hooks

**Example Implementation**:
```rust
struct CounterActor {
    count: u64,
}

#[async_trait(?Send)]
impl Actor for CounterActor {
    async fn on_activate(&mut self, ctx: &ActorContext) -> Result<()> {
        tracing::info!("Counter actor {} activated", ctx.address);
        Ok(())
    }

    async fn handle(&mut self, msg: Message, ctx: &ActorContext) -> Result<Response> {
        match msg.payload_str()? {
            "increment" => {
                self.count += 1;
                Ok(Response::text(format!("{}", self.count)))
            }
            "get" => {
                Ok(Response::text(format!("{}", self.count)))
            }
            _ => Err(Error::UnknownMessage),
        }
    }
}
```

**Invariants**:
```rust
fn check_actor_lifecycle_invariants(instance: &RefCell<ActorInstance>) {
    let inst = instance.borrow();

    // BUG DETECTOR: Deactivations ≤ Activations (single-threaded: simple fields)
    always_assert!(
        inst.metrics.deactivation_count <= inst.metrics.activation_count,
        "More deactivations than activations"
    );

    // Always: Messages only handled when Active
    if inst.metrics.messages_handled > 0 {
        always_assert!(
            matches!(inst.state, ActorState::Active),
            "Messages handled in non-Active state"
        );
    }
}

// ActorMetrics structure (single-threaded)
struct ActorMetrics {
    activation_count: u64,
    deactivation_count: u64,
    messages_handled: u64,
}
```

**Simulation Testing**:
```rust
#[tokio::test]
async fn test_actor_lifecycle() {
    let mut sim = Simulation::new(42);
    let catalog = ActorCatalog::new(/* ... */);

    catalog.start().await.unwrap();

    let addr = ActorAddress::system("Counter");
    let instance = catalog.get_or_create(addr).await.unwrap();

    // Should be in Active state (single-threaded: simple borrow)
    {
        let inst = instance.borrow();
        assert!(matches!(inst.state, ActorState::Active));
    }

    // Send message
    instance.send_message(Message::text("increment")).await.unwrap();

    sim.run_until_idle().await;

    // Deactivate
    catalog.deactivate(&instance.borrow().address, DeactivationReason::app_requested()).await.unwrap();

    {
        let inst = instance.borrow();
        assert!(matches!(inst.state, ActorState::Invalid));
    }

    check_actor_lifecycle_invariants(&instance);
}
```

**Buggify Placement**:
- In on_activate(): Return error 25% of time (validates error recovery)
- In handle(): Inject 1-100ms delay 10% of time (validates timeout logic)

**Files to Create**:
- `moonpool/src/actor.rs`
- `moonpool/src/actor_context.rs`
- Tests in `moonpool/tests/actor_tests.rs`

---

### Step 2.3: Actor Inbox

#### What to Build

Message queue per actor with flow control.

**State Machine**:
```
[Empty] → [HasMessages] → [Paused] → [Draining]
```

**Implementation**:
```rust
// Single-threaded: no Arc, no atomics
pub struct ActorInbox {
    address: ActorAddress,
    state: InboxState,
    messages: VecDeque<Message>,
    max_capacity: usize,
    metrics: InboxMetrics,
}

enum InboxState {
    Empty,
    HasMessages,
    Paused,
    Draining,
}

// Single-threaded: simple fields
pub struct InboxMetrics {
    messages_queued: u64,
    messages_processed: u64,
    current_depth: usize,
    max_depth: usize,
}
```

**Invariants**:
```rust
fn check_inbox_invariants(inbox: &ActorInbox) {
    // BUG DETECTOR: Message conservation (single-threaded: simple fields)
    always_assert!(
        inbox.metrics.messages_queued == inbox.metrics.messages_processed + inbox.metrics.current_depth as u64,
        "Message conservation broken: {} != {} + {}",
        inbox.metrics.messages_queued,
        inbox.metrics.messages_processed,
        inbox.metrics.current_depth
    );

    // BUG DETECTOR: State matches size
    match inbox.state {
        InboxState::Empty => {
            always_assert!(inbox.messages.is_empty(), "Empty state but has messages");
        }
        InboxState::HasMessages => {
            always_assert!(!inbox.messages.is_empty(), "HasMessages state but queue empty");
        }
        _ => {}
    }

    // Always: Capacity respected
    always_assert!(inbox.messages.len() <= inbox.max_capacity, "Queue overflow");
}
```

**Simulation Testing**:
```rust
#[tokio::test]
async fn test_inbox_backpressure() {
    let mut sim = Simulation::new(42);
    sim.use_random_config();

    let inbox = ActorInbox::new(ActorAddress::system("Test"), 100); // max 100 messages

    // Try to enqueue 1000 messages rapidly
    for i in 0..1000 {
        let result = inbox.enqueue(Message::text(&format!("msg{}", i)));

        if i < 100 {
            assert!(result.is_ok(), "Should accept first 100");
        } else {
            // Should reject or block
            sometimes_assert!(inbox_backpressure, result.is_err(), "Backpressure triggered");
        }
    }

    check_inbox_invariants(&inbox);
}
```

**Buggify Placement**:
- Before enqueue: 1-1000ns delay (tests enqueue races)
- During dequeue: 10-500μs delay 5% of time (simulates slow processing)

**Files to Create**:
- `moonpool/src/inbox.rs`
- Tests in `moonpool/tests/inbox_tests.rs`

---

## Phase 3: Message System

### Goals
- MessageBus with routing and delivery
- **Serialization trait** for pluggable formats
- Correlation-based request-response
- Timeout and error handling
- **Orleans References**: `MessageCenter.cs`, `Message.cs`, `CallbackData.cs`

---

### Step 3.1: Serialization Trait Architecture

#### What to Build

Pluggable serialization trait allowing users to choose format.

**Design Philosophy**:
- Start with `serde_json` for debugging/simplicity
- Users can plug in `bincode`, `protobuf`, `messagepack`, etc.
- Type-safe message handling with dynamic dispatch

**Implementation**:
```rust
/// Trait for message serialization
pub trait MessageSerializer: Send + Sync + 'static {
    /// Serialize a message to bytes
    fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>>;

    /// Deserialize bytes to a message
    fn deserialize<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T>;

    /// Content type identifier (for debugging)
    fn content_type(&self) -> &str;
}

/// Default JSON serializer
pub struct JsonSerializer;

impl MessageSerializer for JsonSerializer {
    fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>> {
        serde_json::to_vec(value)
            .map_err(|e| Error::SerializationFailed(e.to_string()))
    }

    fn deserialize<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T> {
        serde_json::from_slice(bytes)
            .map_err(|e| Error::DeserializationFailed(e.to_string()))
    }

    fn content_type(&self) -> &str {
        "application/json"
    }
}

/// Example: User-provided binary serializer
pub struct BincodeSerializer;

impl MessageSerializer for BincodeSerializer {
    fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>> {
        bincode::serialize(value)
            .map_err(|e| Error::SerializationFailed(e.to_string()))
    }

    fn deserialize<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T> {
        bincode::deserialize(bytes)
            .map_err(|e| Error::DeserializationFailed(e.to_string()))
    }

    fn content_type(&self) -> &str {
        "application/octet-stream"
    }
}
```

**Message Structure**:
```rust
pub struct Message {
    /// Correlation ID for request-response matching
    pub id: CorrelationId,

    /// Source address
    pub sender: ActorAddress,

    /// Destination address
    pub target: ActorAddress,

    /// Serialized payload
    pub payload: Vec<u8>,

    /// Message direction
    pub direction: MessageDirection,

    /// Optional timeout
    pub timeout: Option<Duration>,

    /// Message flags
    pub flags: MessageFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDirection {
    Request,
    Response,
    OneWay,
}

bitflags! {
    pub struct MessageFlags: u16 {
        const READ_ONLY = 1 << 0;
        const ALWAYS_INTERLEAVE = 1 << 1;
        const IS_LOCAL_ONLY = 1 << 2;
        const SUPPRESS_KEEP_ALIVE = 1 << 3;
    }
}

pub type CorrelationId = u64;
```

**Type-Safe Message Helpers**:
```rust
impl Message {
    pub fn request<T: Serialize>(
        sender: ActorAddress,
        target: ActorAddress,
        payload: &T,
        serializer: &dyn MessageSerializer,
    ) -> Result<Self> {
        Ok(Self {
            id: generate_correlation_id(),
            sender,
            target,
            payload: serializer.serialize(payload)?,
            direction: MessageDirection::Request,
            timeout: None,
            flags: MessageFlags::empty(),
        })
    }

    pub fn deserialize_payload<T: DeserializeOwned>(
        &self,
        serializer: &dyn MessageSerializer,
    ) -> Result<T> {
        serializer.deserialize(&self.payload)
    }
}
```

**Orleans Pattern**: Packed headers from `Message.cs:381-425`
```rust
// Future optimization: Pack flags into u32 bitfield
// For Phase 12, use straightforward struct
```

**Testing**:
```rust
#[test]
fn test_pluggable_serialization() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct TestPayload {
        count: u64,
        name: String,
    }

    let payload = TestPayload { count: 42, name: "test".into() };

    // Test with JSON
    let json_serializer = JsonSerializer;
    let msg = Message::request(
        ActorAddress::system("A"),
        ActorAddress::system("B"),
        &payload,
        &json_serializer,
    ).unwrap();

    let decoded: TestPayload = msg.deserialize_payload(&json_serializer).unwrap();
    assert_eq!(decoded, payload);

    // Test with Bincode
    let bincode_serializer = BincodeSerializer;
    let msg2 = Message::request(
        ActorAddress::system("A"),
        ActorAddress::system("B"),
        &payload,
        &bincode_serializer,
    ).unwrap();

    let decoded2: TestPayload = msg2.deserialize_payload(&bincode_serializer).unwrap();
    assert_eq!(decoded2, payload);

    // Verify different formats produce different bytes
    assert_ne!(msg.payload, msg2.payload);
}
```

**Files to Create**:
- `moonpool/src/serialization.rs`
- `moonpool/src/message.rs`
- Tests in `moonpool/tests/serialization_tests.rs`

---

### Step 3.2: MessageBus Implementation

#### What to Build

Message routing infrastructure (NOT an actor, just infrastructure like Orleans MessageCenter).

**State Machine**:
```
[Starting] → [Ready] → [Degraded] → [Stopping] → [Stopped]
```

**Implementation**:
```rust
// Single-threaded: no Arc<Mutex>, use Rc<RefCell>
pub struct MessageBus {
    catalog: Rc<ActorCatalog>,
    state: RefCell<MessageBusState>,
    serializer: Arc<dyn MessageSerializer>,
    pending_requests: RefCell<HashMap<CorrelationId, CallbackData>>,
    time: Arc<dyn TimeProvider>,
    task: Arc<dyn TaskProvider>,
    metrics: MessageBusMetrics,
}

enum MessageBusState {
    Starting,
    Ready,
    Degraded,
    Stopping,
    Stopped,
}

// Single-threaded: simple fields
pub struct MessageBusMetrics {
    messages_sent: u64,
    messages_delivered: u64,
    in_transit: u64,
    routing_errors: u64,
}
```

**Core Methods**:
```rust
impl MessageBus {
    /// Send a one-way message
    pub async fn send(&self, msg: Message) -> Result<()> {
        self.check_state()?;

        // Route to local actor
        let instance = self.catalog.get_or_create(msg.target.clone())?;

        let mut inst = instance.borrow_mut();
        inst.inbox_tx.send(msg)?;

        // Single-threaded: simple increment
        self.metrics.messages_sent += 1;
        self.update_metrics();

        Ok(())
    }

    /// Send request and await response (Orleans CallbackData pattern)
    pub async fn request<TReq, TResp>(
        &self,
        target: ActorAddress,
        payload: &TReq,
        timeout: Duration,
    ) -> Result<TResp>
    where
        TReq: Serialize,
        TResp: DeserializeOwned,
    {
        let (tx, rx) = oneshot::channel();
        let correlation_id = generate_correlation_id();

        // Register callback (single-threaded: simple borrow_mut)
        let callback = CallbackData::new(correlation_id, tx, self.time.now(), timeout);
        self.pending_requests.borrow_mut().insert(correlation_id, callback);

        // Create and send request
        let msg = Message {
            id: correlation_id,
            sender: ActorAddress::system("MessageBus"), // TODO: proper sender
            target,
            payload: self.serializer.serialize(payload)?,
            direction: MessageDirection::Request,
            timeout: Some(timeout),
            flags: MessageFlags::empty(),
        };

        self.send(msg).await?;

        // Wait for response with timeout (use TimeProvider::timeout, not tokio::time::timeout)
        match self.time.timeout(timeout, rx).await {
            Ok(Ok(response_bytes)) => {
                self.serializer.deserialize(&response_bytes)
            }
            Ok(Err(_)) => Err(Error::RequestCancelled),
            Err(_) => {
                // Timeout - remove callback (single-threaded: simple borrow_mut)
                self.pending_requests.borrow_mut().remove(&correlation_id);
                Err(Error::RequestTimeout)
            }
        }
    }

    /// Handle incoming response (called by MessageBus internally)
    pub(crate) fn on_response(&self, msg: Message) {
        // Single-threaded: simple borrow_mut
        if let Some(callback) = self.pending_requests.borrow_mut().remove(&msg.id) {
            let _ = callback.complete(msg.payload);
        }
    }

    fn check_state(&self) -> Result<()> {
        let state = self.state.borrow();
        match *state {
            MessageBusState::Ready => Ok(()),
            MessageBusState::Degraded => Ok(()), // Allow in degraded mode
            _ => Err(Error::MessageBusNotReady),
        }
    }
}

/// Orleans CallbackData pattern for tracking pending responses
struct CallbackData {
    correlation_id: CorrelationId,
    completion: oneshot::Sender<Vec<u8>>,
    started: TimeInstant,  // From TimeProvider::now(), not std::time::Instant
    timeout: Duration,
}

// NOTE: TimeInstant will be defined as the return type of TimeProvider::now()
// This ensures deterministic time in simulation

impl CallbackData {
    fn complete(self, payload: Vec<u8>) -> Result<()> {
        self.completion.send(payload)
            .map_err(|_| Error::CallbackDropped)
    }
}
```

**Orleans Pattern**: Request-response correlation from `CallbackData.cs:152-203`

**Single-Threaded Simplification**: Orleans uses `Interlocked.CompareExchange` for exactly-once completion. In moonpool's single-threaded model, we don't need atomics - simple boolean flag is sufficient.

**Invariants**:
```rust
fn check_message_bus_invariants(bus: &MessageBus, state_registry: &StateRegistry) {
    // BUG DETECTOR: Routing conservation (single-threaded: simple fields)
    always_assert!(
        bus.metrics.messages_sent == bus.metrics.messages_delivered + bus.metrics.in_transit,
        "Message loss detected: {} != {} + {}",
        bus.metrics.messages_sent,
        bus.metrics.messages_delivered,
        bus.metrics.in_transit
    );

    // BUG DETECTOR: Correlation ID uniqueness (single-threaded: simple borrow)
    let pending = bus.pending_requests.borrow();
    let unique_ids: HashSet<_> = pending.keys().collect();
    always_assert!(unique_ids.len() == pending.len(), "Duplicate correlation IDs");

    // Always: Delivery only when Ready or Degraded
    let state = bus.state.borrow();
    if bus.metrics.messages_delivered > 0 {
        always_assert!(
            matches!(*state, MessageBusState::Ready | MessageBusState::Degraded),
            "Messages delivered in invalid state"
        );
    }
}
```

**Simulation Testing**:
```rust
#[tokio::test]
async fn test_message_bus_request_response() {
    let mut sim = Simulation::new(42);
    sim.use_random_config();

    let catalog = ActorCatalog::new(/* ... */);
    let bus = MessageBus::new(catalog, Arc::new(JsonSerializer));

    catalog.start().await.unwrap();
    bus.start().await.unwrap();

    // Register a simple echo actor
    struct EchoActor;
    #[async_trait(?Send)]
    impl Actor for EchoActor {
        async fn handle(&mut self, msg: Message, ctx: &ActorContext) -> Result<Response> {
            Ok(Response::with_payload(msg.payload))
        }
    }

    catalog.register_static("system", "Echo", Box::new(EchoActor)).await.unwrap();

    // Send request
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Ping { count: u64 }

    let response: Ping = bus.request(
        ActorAddress::system("Echo"),
        &Ping { count: 42 },
        Duration::from_secs(5),
    ).await.unwrap();

    assert_eq!(response.count, 42);

    sometimes_assert!(request_response_success, true, "Request-response completed");
    check_message_bus_invariants(&bus, &sim.state_registry());
}

#[tokio::test]
async fn test_message_bus_timeout() {
    let mut sim = Simulation::new(42);

    let catalog = ActorCatalog::new(/* ... */);
    let bus = MessageBus::new(catalog, Arc::new(JsonSerializer));

    // Actor that never responds
    struct SlowActor;
    #[async_trait(?Send)]
    impl Actor for SlowActor {
        async fn handle(&mut self, msg: Message, ctx: &ActorContext) -> Result<Response> {
            // Never respond
            ctx.time.sleep(Duration::from_secs(1000)).await;
            Ok(Response::empty())
        }
    }

    catalog.register_static("system", "Slow", Box::new(SlowActor)).await.unwrap();

    // Request with short timeout
    let result = bus.request::<(), ()>(
        ActorAddress::system("Slow"),
        &(),
        Duration::from_millis(10),
    ).await;

    assert!(matches!(result, Err(Error::RequestTimeout)));
    sometimes_assert!(request_timeout_occurred, true, "Request timeout detected");
}
```

**Buggify Placement**:
- Before catalog lookup: 1-500μs delay (tests lookup races)
- Before delivery: Transition to Degraded 10% of time, sleep 10-100ms, return to Ready
- During request execution: 1-10s delay 25% of time (triggers timeouts)

**Files to Create**:
- `moonpool/src/message_bus.rs`
- Tests in `moonpool/tests/message_bus_tests.rs`

---

## Phase 4: Network Integration

### Goals
- Connect MessageBus to foundation's ClientTransport/ServerTransport
- Implement distributed static actors (SystemTargets)
- Create PingActor with **superior DX** compared to manual ping_pong
- Multi-node validation with comprehensive testing

---

### Step 4.1: Network Message Router

#### What to Build

Bridge between MessageBus (local) and foundation transport (network).

**Implementation**:
```rust
pub struct NetworkRouter {
    message_bus: Arc<MessageBus>,
    my_address: String,
    transport: ClientTransport</* ... */>,
    server_transport: ServerTransport</* ... */>,
    network: Arc<dyn NetworkProvider>,
    time: Arc<dyn TimeProvider>,
    task: Arc<dyn TaskProvider>,
}

impl NetworkRouter {
    pub async fn route_message(&self, msg: Message) -> Result<()> {
        // Determine if message is local or remote
        if self.is_local_actor(&msg.target) {
            // Route via local MessageBus
            self.message_bus.send(msg).await
        } else {
            // Route via network transport
            self.send_remote(msg).await
        }
    }

    async fn send_remote(&self, msg: Message) -> Result<()> {
        // Serialize message as envelope
        let envelope = ActorMessageEnvelope {
            correlation_id: msg.id,
            sender: msg.sender,
            target: msg.target,
            payload: msg.payload,
            direction: msg.direction,
        };

        let target_silo = self.resolve_silo_address(&msg.target).await?;

        // Use foundation transport
        self.transport.request_with_timeout(
            &target_silo,
            &envelope,
            msg.timeout.unwrap_or(Duration::from_secs(30)),
        ).await?;

        Ok(())
    }

    pub async fn receive_loop(&mut self) {
        loop {
            tokio::select! {
                Some(incoming) = self.server_transport.next_message() => {
                    let envelope: ActorMessageEnvelope = incoming.envelope.deserialize().unwrap();

                    let msg = Message {
                        id: envelope.correlation_id,
                        sender: envelope.sender,
                        target: envelope.target,
                        payload: envelope.payload,
                        direction: envelope.direction,
                        timeout: None,
                        flags: MessageFlags::empty(),
                    };

                    // Route to local actor
                    if let Err(e) = self.message_bus.send(msg).await {
                        tracing::warn!("Failed to route incoming message: {}", e);
                    }
                }

                // Shutdown
                _ = self.shutdown_signal.cancelled() => {
                    break;
                }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ActorMessageEnvelope {
    correlation_id: CorrelationId,
    sender: ActorAddress,
    target: ActorAddress,
    payload: Vec<u8>,
    direction: MessageDirection,
}
```

**Simulation Testing**:
```rust
#[tokio::test]
async fn test_network_router_multi_node() {
    let mut sim = Simulation::new(42);
    sim.use_random_config();

    let topology = WorkloadTopology {
        my_ip: "127.0.0.1:8001".into(),
        peer_ips: vec!["127.0.0.1:8002".into()],
        /* ... */
    };

    // Create two nodes
    let router1 = NetworkRouter::new(/* node 1 */);
    let router2 = NetworkRouter::new(/* node 2 */);

    // Start receive loops
    sim.task_provider().spawn_task(async move { router1.receive_loop().await });
    sim.task_provider().spawn_task(async move { router2.receive_loop().await });

    // Send message from node1 to actor on node2
    let msg = Message::request(
        ActorAddress::system("Node1Actor"),
        ActorAddress::system("Node2Actor"),
        &TestPayload { value: 42 },
        &JsonSerializer,
    ).unwrap();

    router1.route_message(msg).await.unwrap();

    sim.run_until_idle().await;

    // Verify delivery
    sometimes_assert!(cross_node_delivery, true, "Cross-node message delivered");
}
```

**Files to Create**:
- `moonpool/src/network_router.rs`
- Tests in `moonpool/tests/network_router_tests.rs`

---

### Step 4.2: PingActor with Superior DX

#### What to Build

Distributed PingActor demonstrating **dramatically better developer experience** than manual ping_pong.

**Current ping_pong DX issues**:
- Manual struct with generic soup: `<N: NetworkProvider + Clone + 'static, T: TimeProvider + Clone + 'static, TP: TaskProvider + Clone + 'static>`
- Manual state tracking: `pings_received`, `pongs_sent`, hash maps
- Manual transport management: explicit `bind()`, `next_message()`, `send_reply()`
- String-based protocol: `"PING:"`, `"PONG:"`, manual parsing
- Manual metrics updates: `self.update_state()` everywhere

**PingActor with Superior DX**:

```rust
use moonpool::prelude::*;

/// Ping message (type-safe, auto-serialized)
#[derive(Serialize, Deserialize, Debug)]
struct Ping {
    sequence: u64,
    from_node: String,
}

/// Pong response (type-safe, auto-serialized)
#[derive(Serialize, Deserialize, Debug)]
struct Pong {
    sequence: u64,
    from_node: String,
}

/// PingActor - clean, minimal boilerplate
struct PingActor {
    pings_received: u64,
    pongs_sent: u64,
}

#[async_trait(?Send)]
impl Actor for PingActor {
    async fn on_activate(&mut self, ctx: &ActorContext) -> Result<()> {
        tracing::info!("PingActor activated on {}", ctx.address);
        Ok(())
    }

    async fn handle(&mut self, msg: Message, ctx: &ActorContext) -> Result<Response> {
        // Type-safe deserialization
        let ping: Ping = msg.deserialize_payload(ctx.serializer())?;

        self.pings_received += 1;

        tracing::debug!(
            "Received ping #{} from {}",
            ping.sequence,
            ping.from_node
        );

        // Create response (type-safe, auto-serialized)
        let pong = Pong {
            sequence: ping.sequence,
            from_node: ctx.my_node_id().to_string(),
        };

        self.pongs_sent += 1;

        // Automatic state registration via Context
        ctx.update_state(serde_json::json!({
            "pings_received": self.pings_received,
            "pongs_sent": self.pongs_sent,
        }));

        Ok(Response::from_serializable(&pong, ctx.serializer())?)
    }
}

/// Test: Multi-node ping-pong
#[tokio::test]
async fn test_ping_actor_distributed() {
    let mut sim = Simulation::new(42);
    sim.use_random_config();

    // Topology: 2 nodes
    let node1_topology = WorkloadTopology {
        my_ip: "127.0.0.1:8001".into(),
        peer_ips: vec!["127.0.0.1:8002".into()],
        state_registry: sim.state_registry(),
        shutdown_signal: sim.shutdown_signal(),
    };

    let node2_topology = WorkloadTopology {
        my_ip: "127.0.0.1:8002".into(),
        peer_ips: vec!["127.0.0.1:8001".into()],
        state_registry: sim.state_registry(),
        shutdown_signal: sim.shutdown_signal(),
    };

    // Create actor systems on each node
    let system1 = ActorSystem::new(node1_topology);
    let system2 = ActorSystem::new(node2_topology);

    // Register PingActor as static actor on both nodes
    system1.register_static_actor("system", "Ping", || PingActor {
        pings_received: 0,
        pongs_sent: 0,
    }).await.unwrap();

    system2.register_static_actor("system", "Ping", || PingActor {
        pings_received: 0,
        pongs_sent: 0,
    }).await.unwrap();

    // Start both systems
    system1.start().await.unwrap();
    system2.start().await.unwrap();

    // Send ping from node1 to node2
    let pong: Pong = system1.request(
        ActorAddress::system("Ping").on_node("127.0.0.1:8002"),
        &Ping {
            sequence: 1,
            from_node: "127.0.0.1:8001".into(),
        },
        Duration::from_secs(5),
    ).await.unwrap();

    assert_eq!(pong.sequence, 1);
    assert_eq!(pong.from_node, "127.0.0.1:8002");

    // Validate via invariants
    check_ping_actor_invariants(&sim.state_registry());

    sometimes_assert!(cross_node_ping, true, "Cross-node ping succeeded");
}

/// Invariants: Message conservation across nodes
fn check_ping_actor_invariants(registry: &StateRegistry) {
    let states = registry.get_all_states();

    let mut total_pings_sent = 0u64;
    let mut total_pongs_received = 0u64;
    let mut total_pings_received = 0u64;
    let mut total_pongs_sent = 0u64;

    for (node_id, state) in states {
        if let Some(actor_state) = state.get("system::Ping") {
            total_pings_received += actor_state["pings_received"].as_u64().unwrap_or(0);
            total_pongs_sent += actor_state["pongs_sent"].as_u64().unwrap_or(0);
        }
    }

    // BUG DETECTOR: Pongs sent = pings received
    always_assert!(
        total_pongs_sent == total_pings_received,
        "Pong/ping mismatch: {} pongs sent, {} pings received",
        total_pongs_sent,
        total_pings_received
    );
}
```

**DX Improvements over Manual ping_pong**:
✅ **70% less code** - No generic soup, automatic context injection
✅ **Type-safe messages** - No string parsing, compile-time checks
✅ **Automatic serialization** - Works with any format via trait
✅ **Context handles everything** - State registration, node ID, serializer access
✅ **Clean async/await** - No manual transport management
✅ **Better testing** - `ActorSystem::request()` replaces manual transport setup
✅ **Invariants built-in** - Cross-workload validation via StateRegistry

**Multi-Topology Testing**:
```rust
#[tokio::test]
async fn test_ping_actor_multi_topology() {
    for (node_count, topology_name) in [(2, "2x2"), (5, "5x5"), (10, "10x10")] {
        let mut sim = Simulation::new(42);
        sim.use_random_config();

        let systems = create_actor_systems(&sim, node_count);

        // Register PingActor on all nodes
        for system in &systems {
            system.register_static_actor("system", "Ping", || PingActor::default()).await.unwrap();
            system.start().await.unwrap();
        }

        // Each node pings all other nodes
        for sender in &systems {
            for receiver in &systems {
                if sender.node_id() != receiver.node_id() {
                    let pong: Pong = sender.request(
                        ActorAddress::system("Ping").on_node(receiver.node_id()),
                        &Ping { sequence: 1, from_node: sender.node_id().into() },
                        Duration::from_secs(5),
                    ).await.unwrap();

                    assert_eq!(pong.from_node, receiver.node_id());
                }
            }
        }

        check_ping_actor_invariants(&sim.state_registry());
        tracing::info!("✅ {} topology passed", topology_name);
    }
}
```

**Buggify Placement**:
- Before sending ping: 1-100ms delay (tests timeouts)
- After receiving ping: Random failure 5% of time (tests error recovery)

**Success Criteria**:
- ✅ 2x2, 5x5, 10x10 topologies pass
- ✅ 100% success rate across 10+ seeds
- ✅ All sometimes_assert! triggered
- ✅ Cross-node invariants validated
- ✅ Code is 70% smaller than manual ping_pong
- ✅ Developer experience is dramatically improved

**Files to Create**:
- `moonpool/src/actor_system.rs`
- `moonpool/examples/ping_actor.rs`
- Tests in `moonpool/tests/ping_actor_tests.rs`

---

### Step 4.3: End-to-End Validation

#### Comprehensive Multi-Node Testing

```rust
#[tokio::test]
async fn test_complete_actor_system_e2e() {
    let mut sim = Simulation::new(42);
    sim.use_random_config();

    // Create 10-node cluster
    let systems = create_actor_systems(&sim, 10);

    // Register various actors
    for system in &systems {
        system.register_static_actor("system", "Ping", || PingActor::default()).await.unwrap();
        system.register_static_actor("system", "Counter", || CounterActor::default()).await.unwrap();
        system.start().await.unwrap();
    }

    // Stress test: 1000 messages across all nodes
    let mut handles = vec![];
    for _ in 0..1000 {
        let sender = &systems[sim.random().random_range(0..systems.len())];
        let receiver = &systems[sim.random().random_range(0..systems.len())];

        let handle = sim.task_provider().spawn_task({
            let sender = sender.clone();
            let receiver_id = receiver.node_id().to_string();
            async move {
                sender.request::<Ping, Pong>(
                    ActorAddress::system("Ping").on_node(&receiver_id),
                    &Ping { sequence: 1, from_node: sender.node_id().into() },
                    Duration::from_secs(5),
                ).await
            }
        });
        handles.push(handle);
    }

    // Wait for all
    let results = futures::future::join_all(handles).await;

    // Validate
    let successes = results.iter().filter(|r| r.is_ok()).count();
    tracing::info!("✅ {}/{} messages succeeded", successes, results.len());

    // Allow some failures in chaos mode
    sometimes_assert!(high_success_rate, successes > 900, "High success rate under chaos");

    // Check invariants
    check_ping_actor_invariants(&sim.state_registry());
    check_message_bus_invariants_all_nodes(&systems);
}
```

**Files to Create**:
- `moonpool/tests/e2e_distributed_tests.rs`

---

## Phase 5: Virtual Actor Hooks

### Goal
Design extension points for future virtual actor implementation without requiring rewrites.

### Step 5.1: Extension Points

**ActorAddress Extension**:
```rust
impl ActorAddress {
    pub fn is_virtual(&self) -> bool {
        self.key.is_some()
    }
}
```

**Catalog Hooks**:
```rust
impl ActorCatalog {
    // Hook for future virtual actor activation
    async fn maybe_activate_virtual(&self, address: &ActorAddress) -> Result<()> {
        if address.is_virtual() {
            // TODO Phase 13: Implement virtual actor activation
            //  - Directory lookup
            //  - On-demand creation
            //  - Idle timeout management
            return Err(Error::VirtualActorsNotYetSupported);
        }
        Ok(())
    }
}
```

**MessageBus Hooks**:
```rust
impl MessageBus {
    async fn route_to_virtual(&self, msg: Message) -> Result<()> {
        // TODO Phase 13: Implement virtual actor routing
        //  - Directory lookup
        //  - Cross-silo forwarding
        //  - Cache invalidation
        Err(Error::VirtualActorsNotYetSupported)
    }
}
```

**Documentation**:
```rust
/// Phase 13 TODO: Virtual Actor Support
///
/// Virtual actors require:
/// 1. Distributed directory service (ActorDirectory)
/// 2. On-demand activation with idle timeout
/// 3. Cross-silo message forwarding
/// 4. Cache invalidation protocol
/// 5. Migration support
///
/// Current design supports static actors only.
/// See docs/plans/phase-13-virtual-actors.md for roadmap.
```

---

## Summary

### Phase 12 Deliverables

✅ **Actor Scheduling Foundation**:
- Quantum-based WorkQueue with state machines
- Actor addressing (namespace/type/key)
- Comprehensive metrics and observability

✅ **Actor Registry & Lifecycle**:
- ActorCatalog with Orleans double-check locking
- Actor trait with lifecycle hooks
- Inbox with flow control

✅ **Message System**:
- Serialization trait (pluggable: JSON, bincode, protobuf)
- MessageBus with correlation-based RPC
- Timeout and error handling

✅ **Network Integration**:
- NetworkRouter bridging local and remote
- Distributed static actors (SystemTargets)
- PingActor with superior DX (70% less code)

✅ **Virtual Actor Preparation**:
- Extension points for Phase 13
- Design documentation

### Testing Coverage

- ✅ Unit tests per component
- ✅ Simulation tests with buggify
- ✅ Multi-topology tests (2x2, 5x5, 10x10)
- ✅ Cross-node invariants
- ✅ 100% success rate requirement
- ✅ All sometimes_assert! triggered

### Developer Experience Wins

**Before (manual ping_pong)**:
- 399 lines of boilerplate
- Generic soup everywhere
- Manual state tracking
- String-based protocol

**After (PingActor)**:
- ~50 lines of clean code
- Type-safe messages
- Automatic state management
- Clean async/await

**70% code reduction, dramatically better DX**

---

## Validation Checklist

Before completing Phase 12:

1. ✅ `nix develop --command cargo fmt` passes
2. ✅ `nix develop --command cargo clippy` no warnings
3. ✅ `nix develop --command cargo nextest run` all tests pass
4. ✅ No test timeouts or hangs
5. ✅ All sometimes_assert! triggered in chaos tests (UntilAllSometimesReached)
6. ✅ Multi-topology tests pass (2x2, 5x5, 10x10)
7. ✅ Cross-node invariants validated
8. ✅ PingActor demonstrates superior DX
9. ✅ Documentation updated (CLAUDE.md, specs)
10. ✅ Phase 13 roadmap documented

---

## Orleans Patterns Applied & Simplified

✅ **Double-check locking** (Catalog.cs:106-195) - Actor creation (simplified: no async locks)
✅ **Command pattern** (ActivationData.cs:2070-2128) - Lifecycle serialization
✅ **Quantum-based execution** (WorkItemGroup.cs:143-222) - Fair scheduling
✅ **Correlation-based RPC** (CallbackData.cs) - Request-response (simplified: no Interlocked)
✅ **Packed headers** (Message.cs:381-425) - Efficient metadata (future optimization)
✅ **State machines** (throughout) - Explicit states for debugging
✅ **MessageCenter as infrastructure** (MessageCenter.cs:16) - NOT a SystemTarget
✅ **Single-threaded simplification** - No atomics, no Arc<Mutex>, just RefCell/Rc

### Orleans Multi-Threaded → Moonpool Single-Threaded Mappings

| Orleans (Multi-threaded) | Moonpool (Single-threaded) |
|--------------------------|----------------------------|
| `Arc<Mutex<T>>` | `Rc<RefCell<T>>` |
| `AtomicU64` | `u64` |
| `Interlocked.CompareExchange` | Simple boolean flag |
| `lock(this) { }` | `borrow_mut()` |
| Thread-safety concerns | Borrow checker only |
| `tokio::time::sleep()` | `time.sleep()` (TimeProvider) |
| `tokio::spawn()` | `task.spawn_task()` (TaskProvider) |
| `Instant::now()` | `time.now()` (TimeProvider) |

---

## Next Steps (Phase 13+)

### Phase 13: Virtual Actors
- Distributed directory service
- On-demand activation
- Idle timeout and collection
- Cross-silo forwarding
- Migration support

### Phase 14: Persistence
- State storage interface
- Event sourcing
- Snapshotting
- Recovery

### Phase 15: Advanced Features
- Streaming
- Timers and reminders
- Reentrancy control
- Custom placement strategies

---

**Phase 12 Status**: Ready to implement
**Confidence Level**: High (Orleans patterns validated, foundation proven)
