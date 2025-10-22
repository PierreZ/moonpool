# Data Model: Location-Transparent Distributed Actor System

**Date**: 2025-10-21
**Phase**: 1 - Design & Contracts

## Overview

This document defines the core entities, their relationships, state transitions, and validation rules for the location-transparent distributed actor system.

---

## Core Entities

### 1. ActorId

**Purpose**: Unique identifier for a virtual actor, independent of physical location.

**Structure**:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActorId {
    pub namespace: String,
    pub actor_type: String,
    pub key: String,
}
```

**Fields**:
- `namespace` - Logical namespace for isolation (e.g., "prod", "staging", "tenant-123")
- `actor_type` - Type of actor (e.g., "BankAccount", "User", "Session")
- `key` - Unique key within the namespace+type (e.g., "alice", "account-456")

**Construction Helpers**:
```rust
impl ActorId {
    // Internal constructor (used by ActorSystem)
    pub(crate) fn new(namespace: impl Into<String>, actor_type: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            actor_type: actor_type.into(),
            key: key.into(),
        }
    }

    // Formatted string representation: namespace::actor_type/key
    pub fn to_string(&self) -> String {
        format!("{}::{}/{}", self.namespace, self.actor_type, self.key)
    }

    // Parse from string format (used for logging, debugging)
    pub fn from_string(s: &str) -> Result<Self, ActorIdError> {
        let parts: Vec<&str> = s.split("::").collect();
        if parts.len() != 2 {
            return Err(ActorIdError::InvalidFormat);
        }

        let namespace = parts[0];
        let actor_and_key: Vec<&str> = parts[1].split('/').collect();
        if actor_and_key.len() != 2 {
            return Err(ActorIdError::InvalidFormat);
        }

        Ok(Self::new(namespace, actor_and_key[0], actor_and_key[1]))
    }
}
```

**Validation Rules**:
- `namespace` MUST NOT be empty
- `actor_type` MUST NOT be empty
- `key` MUST NOT be empty
- All fields SHOULD contain only alphanumeric, dash, underscore (for safety in logs/URLs)
- Total length SHOULD NOT exceed 256 characters
- MUST be unique within the (namespace, actor_type) scope

**Invariants**:
- ActorId equality requires all three fields to match
- ActorId can be serialized and deserialized without loss
- Same (namespace, actor_type, key) tuple always refers to same virtual actor

**Namespace Assignment**:
- Namespace is set at ActorRuntime bootstrap (cluster-wide configuration)
- All actors within a cluster share the same namespace
- Cannot send messages across namespaces (namespace boundary = cluster boundary)
- ActorRuntime automatically applies namespace to all ActorIds it creates

**Examples**:
```rust
// ActorRuntime bootstrapped with namespace "prod"
let runtime = ActorRuntime::builder()
    .namespace("prod")
    .listen_addr("127.0.0.1:5000")
    .build()
    .await?;

// get_actor() automatically applies "prod" namespace
let alice = runtime.get_actor("BankAccount", "alice");
// Internally creates: ActorId { namespace: "prod", actor_type: "BankAccount", key: "alice" }
// String format: "prod::BankAccount/alice"

// Different cluster with different namespace
let staging_runtime = ActorRuntime::builder()
    .namespace("staging")
    .listen_addr("127.0.0.1:6000")
    .build()
    .await?;
let alice_staging = staging_runtime.get_actor("BankAccount", "alice");
// String format: "staging::BankAccount/alice"
// This is a DIFFERENT actor than "prod::BankAccount/alice"

// Parse from logs/config (includes namespace)
let actor_id = ActorId::from_string("prod::BankAccount/alice")?;
```

**String Format**: `namespace::actor_type/key`
- `::` separates namespace from actor_type
- `/` separates actor_type from key
- Examples: `prod::BankAccount/alice`, `tenant-acme::User/bob`

**Use Cases**:
- **Environment isolation**: Separate prod/staging/test clusters (different namespaces)
- **Multi-tenancy**: Each tenant gets own cluster with tenant-id namespace
- **Type safety**: Distinguish different actor types in directory
- **Debuggability**: Clear identification in logs and traces
- **Cluster boundaries**: Namespace defines message routing boundary

---

### 2. NodeId

**Purpose**: Identifies a physical node in the cluster by its network address.

**Structure**:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(String);  // Format: "address:port"
```

**Fields**:
- `0: String` - Network address in "host:port" format (e.g., "127.0.0.1:5000", "node1.cluster:8080")

**Construction Helpers**:
```rust
impl NodeId {
    /// Create NodeId from address:port string
    pub fn from(addr: impl Into<String>) -> Result<Self, NodeIdError> {
        let addr = addr.into();
        // Validate format (must contain ':' and valid port)
        Self::validate(&addr)?;
        Ok(Self(addr))
    }

    /// Parse from SocketAddr
    pub fn from_socket_addr(addr: SocketAddr) -> Self {
        Self(addr.to_string())
    }

    /// Get host portion (before colon)
    pub fn host(&self) -> &str {
        self.0.split(':').next().unwrap_or(&self.0)
    }

    /// Get port portion (after colon)
    pub fn port(&self) -> Option<u16> {
        self.0.split(':').nth(1)?.parse().ok()
    }

    /// Convert to SocketAddr for binding/connecting
    pub fn to_socket_addr(&self) -> Result<SocketAddr, NodeIdError> {
        self.0.parse().map_err(|_| NodeIdError::InvalidAddress)
    }
}
```

**Validation Rules**:
- MUST be in "host:port" format with valid separator ':'
- Host can be IPv4 address (e.g., "127.0.0.1"), IPv6 address (e.g., "[::1]"), or hostname (e.g., "node1")
- Port MUST be valid u16 (0-65535)
- MUST be unique within the cluster (no two nodes with same address:port)
- MUST remain stable during node lifetime
- SHOULD be routable/reachable from all cluster nodes

**Examples**:
```rust
// IPv4 address
let node = NodeId::from("127.0.0.1:5000")?;
let node = NodeId::from("192.168.1.100:8080")?;

// Hostname
let node = NodeId::from("node1.cluster.local:5000")?;

// From SocketAddr
let addr = "127.0.0.1:5000".parse::<SocketAddr>()?;
let node = NodeId::from_socket_addr(addr);

// Extract components
assert_eq!(node.host(), "127.0.0.1");
assert_eq!(node.port(), Some(5000));
```

**Invariants**:
- NodeId equality implies same network address (normalized string comparison)
- NodeId persists across actor activations/deactivations
- Network address is resolvable and routable within cluster

---

### 3. CorrelationId

**Purpose**: Matches requests to responses in request-response messaging.

**Structure**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(u64);
```

**Fields**:
- `0: u64` - Monotonically increasing counter per node

**Validation Rules**:
- MUST be unique per sender node (not globally unique)
- MUST NOT be reused within reasonable time window

**Invariants**:
- Response CorrelationId matches request CorrelationId
- Single CorrelationId corresponds to exactly one pending request

---

### 4. Message

**Purpose**: Unit of communication between actors, containing addressing, correlation, and payload.

**Structure**:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub correlation_id: CorrelationId,
    pub direction: Direction,
    pub target_actor: ActorId,
    pub sender_actor: ActorId,
    pub target_node: NodeId,
    pub sender_node: NodeId,
    pub payload: Vec<u8>,          // serde_json serialized
    pub flags: MessageFlags,
    pub time_to_expiry: Option<Instant>,
    pub forward_count: u8,         // Track forwarding hops
}
```

**Fields**:
- `correlation_id` - For matching responses to requests
- `direction` - Request, Response, or OneWay
- `target_actor` - Destination actor ID
- `sender_actor` - Source actor ID
- `target_node` - Physical node hosting target
- `sender_node` - Physical node hosting sender
- `payload` - Application data (JSON serialized)
- `flags` - Control flags (ReadOnly, AlwaysInterleave, etc.)
- `time_to_expiry` - Optional timeout for message delivery
- `forward_count` - Number of times message has been forwarded (prevents loops)

**Validation Rules**:
- Request direction MUST have `time_to_expiry`
- Response correlation_id MUST match pending request
- OneWay MUST NOT have response expectation
- forward_count MUST NOT exceed MAX_FORWARD_COUNT (default: 2)

**Invariants**:
- Response swaps target/sender from request
- Response inherits correlation_id from request

---

### 5. Direction

**Purpose**: Indicates message flow semantics and response expectations.

**Structure**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Request message expecting a response.
    ///
    /// **Semantics**:
    /// - Creates `CallbackData` for response tracking
    /// - Has timeout enforcement (`time_to_expiry` required)
    /// - Response must copy correlation ID
    /// - Blocks caller until response or timeout
    Request,

    /// Response message to a previous request.
    ///
    /// **Semantics**:
    /// - Matches request via correlation ID
    /// - Completes pending `CallbackData`
    /// - Does not have timeout (inherits from request)
    /// - Unblocks waiting caller
    Response,

    /// One-way message with no response expected.
    ///
    /// **Semantics**:
    /// - No `CallbackData` created
    /// - No timeout enforcement
    /// - Fire-and-forget (caller doesn't wait)
    /// - Best-effort delivery
    OneWay,
}
```

**State Transitions**:
```
Request → Response  (matching correlation_id)
OneWay → (terminal, no response)
```

**Usage Implications**:
- Request: `MessageBus::call(message) -> Future<Response>`
- Response: Internal routing, not user-facing
- OneWay: `MessageBus::send(message) -> Result<()>`

---

### 6. MessageFlags

**Purpose**: Control message processing behavior.

**Structure**:
```rust
bitflags! {
    #[derive(Serialize, Deserialize)]
    pub struct MessageFlags: u16 {
        const READ_ONLY = 1 << 0;          // Doesn't mutate state
        const ALWAYS_INTERLEAVE = 1 << 1;  // Can execute concurrently
        const IS_LOCAL_ONLY = 1 << 2;      // Cannot forward to another node
        const SUPPRESS_KEEP_ALIVE = 1 << 3; // Don't extend actor lifetime
    }
}
```

**Validation Rules**:
- ALWAYS_INTERLEAVE implies READ_ONLY (conceptually)
- IS_LOCAL_ONLY prevents message forwarding

---

### 7. ActorAddress

**Purpose**: Complete location information for an actor.

**Structure**:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorAddress {
    pub actor_id: ActorId,
    pub node_id: NodeId,
    pub activation_time: Instant,
}
```

**Fields**:
- `actor_id` - Virtual actor identifier
- `node_id` - Physical node hosting actor
- `activation_time` - When actor was activated (for cache staleness detection)

**Validation Rules**:
- `node_id` MUST be valid cluster member
- `activation_time` MUST be monotonically increasing per actor_id

**Invariants**:
- Same actor_id CAN have different addresses over time (migration/reactivation)
- Same actor_id MUST NOT have multiple addresses simultaneously (single activation)

---

### 8. ActivationState

**Purpose**: Actor lifecycle state machine.

**Structure**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationState {
    Creating,     // Instance created, not yet activated
    Activating,   // OnActivateAsync() in progress
    Valid,        // Ready to process messages
    Deactivating, // OnDeactivateAsync() in progress
    Invalid,      // Disposed, no longer usable
}
```

**State Transitions**:
```
Creating → Activating → Valid → Deactivating → Invalid
         ↓
         Deactivating (activation failed)
```

**Validation Rules**:
- MUST NOT process messages unless state == Valid
- MUST NOT transition backward (except activation failure)
- Transitions guarded by `RefCell<ActivationState>` (single-threaded)

**Invariants**:
- Actor in Valid state has completed on_activate() successfully
- Actor in Invalid state cannot transition to any other state

---

### 9. ActorContext

**Purpose**: Per-actor state and metadata.

**Structure**:
```rust
pub struct ActorContext<A: Actor> {
    pub actor_id: ActorId,
    pub node_id: NodeId,
    pub state: RefCell<ActivationState>,
    pub message_queue: RefCell<VecDeque<Message>>,
    pub actor_instance: RefCell<A>,
    pub activation_time: Instant,
    pub last_message_time: RefCell<Instant>,
}
```

**Fields**:
- `actor_id` - Actor's unique identifier
- `node_id` - Hosting node
- `state` - Current lifecycle state (interior mutability via RefCell)
- `message_queue` - Unbounded FIFO queue for incoming messages
- `actor_instance` - User-defined actor implementation
- `activation_time` - When actor was activated
- `last_message_time` - For idle timeout detection

**Validation Rules**:
- message_queue can only be dequeued when state == Valid
- actor_instance MUST NOT be accessed outside message processing
- state transitions MUST follow ActivationState rules

**Interior Mutability Pattern** (single-threaded):
```rust
// Borrow state (single-threaded, no Mutex needed)
let state = context.state.borrow();
if *state == ActivationState::Valid {
    // Process message
}

// Mutably borrow actor instance
let mut actor = context.actor_instance.borrow_mut();
actor.deposit(100).await?;
```

**Invariants**:
- Single message processed at a time (sequential processing)
- Queue never drops messages (unbounded, monitored)
- last_message_time updated on every message processing

---

### 10. Directory

**Purpose**: Maps ActorId to NodeId for location lookup.

**Conceptual Structure**:
```rust
pub trait Directory {
    async fn lookup(&self, actor_id: &ActorId) -> Option<NodeId>;
    async fn register(&mut self, actor_id: ActorId, node_id: NodeId) -> Result<()>;
    async fn unregister(&mut self, actor_id: &ActorId) -> Result<()>;
}
```

**Internal State** (SimpleDirectory implementation):
```rust
pub struct SimpleDirectory {
    entries: HashMap<ActorId, NodeId>,
    local_cache: HashMap<ActorId, (NodeId, Instant)>,  // (node, cached_at)
    node_load: HashMap<NodeId, usize>,  // For placement algorithm
}
```

**Validation Rules**:
- ActorId MUST map to at most one NodeId (single activation)
- Concurrent registrations MUST be serialized (first wins)
- Unregister MUST only remove if currently registered

**Invariants**:
- Lookup returns None if actor not activated
- Register idempotent for same (actor_id, node_id) pair
- Eventually all nodes converge to same actor location (eventual consistency)

---

### 11. PlacementDecision

**Purpose**: Result of directory placement algorithm.

**Structure**:
```rust
#[derive(Debug, Clone)]
pub enum PlacementDecision {
    AlreadyRegistered(NodeId),         // Actor already active elsewhere
    PlaceOnNode(NodeId),               // Place on this node
    Race { winner: NodeId, loser: NodeId },  // Concurrent activation detected
}
```

**Usage**:
- `AlreadyRegistered`: Forward message to existing activation
- `PlaceOnNode`: Create activation on specified node
- `Race`: First registration wins, other deactivates

---

### 12. ActivationDirectory

**Purpose**: Thread-safe local registry mapping ActorId to active ActorContext instances on this node.

**Structure**:
```rust
pub struct ActivationDirectory<A: Actor> {
    activations: RefCell<HashMap<ActorId, Arc<ActorContext<A>>>>,
    count: Cell<usize>,
}
```

**Fields**:
- `activations` - Map from ActorId to ActorContext (RefCell for interior mutability, single-threaded)
- `count` - Cached count for fast access without HashMap iteration

**Methods**:
```rust
impl<A: Actor> ActivationDirectory<A> {
    /// Find activation by actor ID (returns None if not found)
    pub fn find_activation(&self, actor_id: &ActorId) -> Option<Arc<ActorContext<A>>> {
        self.activations.borrow().get(actor_id).cloned()
    }

    /// Record new activation in directory
    pub fn record_activation(&self, context: Arc<ActorContext<A>>) -> bool {
        let mut activations = self.activations.borrow_mut();
        if activations.insert(context.actor_id.clone(), context).is_none() {
            self.count.set(self.count.get() + 1);
            true
        } else {
            false  // Already existed
        }
    }

    /// Remove activation from directory
    pub fn remove_activation(&self, actor_id: &ActorId) -> bool {
        if self.activations.borrow_mut().remove(actor_id).is_some() {
            self.count.set(self.count.get().saturating_sub(1));
            true
        } else {
            false
        }
    }

    /// Get count of active actors (O(1))
    pub fn count(&self) -> usize {
        self.count.get()
    }
}
```

**Validation Rules**:
- MUST NOT record duplicate ActorId
- MUST remove exact ActorContext instance (not just any with same ActorId)
- Count MUST match HashMap size

**Invariants**:
- Actor in directory ⟺ actor.state != Invalid
- count() == activations.len() (maintained via Cell updates)
- Single-threaded access via RefCell (no races in current_thread runtime)

**Pattern**: Separate counter for fast `count()` without HashMap iteration (Orleans pattern)

---

### 13. ActorCatalog

**Purpose**: Orchestrates actor lifecycle on a single node with double-check locking.

**Structure**:
```rust
pub struct ActorCatalog<A: Actor> {
    activation_directory: ActivationDirectory<A>,
    activation_lock: RefCell<()>,  // Coarse lock for get-or-create
    node_id: NodeId,
    directory: Arc<dyn Directory>,
}
```

**Fields**:
- `activation_directory` - Local registry of active actors (ActivationDirectory)
- `activation_lock` - Coarse lock for double-check pattern (RefCell guard)
- `node_id` - This node's identifier
- `directory` - Reference to distributed directory service

**Methods** (Orleans double-check locking pattern):
```rust
impl<A: Actor> ActorCatalog<A> {
    /// Get existing activation or create new one (double-check locking)
    pub fn get_or_create_activation(&self, actor_id: ActorId) -> Result<Arc<ActorContext<A>>> {
        // Fast path: check without lock
        if let Some(activation) = self.activation_directory.find_activation(&actor_id) {
            return Ok(activation);
        }

        // Slow path: acquire lock
        let _guard = self.activation_lock.borrow_mut();

        // Double-check under lock (race protection)
        if let Some(activation) = self.activation_directory.find_activation(&actor_id) {
            return Ok(activation);
        }

        // Create new activation
        let context = Arc::new(ActorContext::new(actor_id.clone(), self.node_id.clone()));
        self.activation_directory.record_activation(context.clone());

        Ok(context)
    }
}
```

**Validation Rules**:
- Actor MUST be in activation_directory if state != Invalid
- Actor MUST NOT be in directory if deactivation completed
- get_or_create MUST NOT create duplicates (double-check protection)

**Invariants**:
- Double-check locking prevents duplicate activations
- Activation happens outside lock (avoid holding lock during I/O)
- Single-threaded execution via RefCell (no Send/Sync requirements)

**Pattern** (from Orleans `Catalog.cs:106-195`):
1. **Check without lock** (fast path for existing activations)
2. **Acquire lock** (serialize activation creation)
3. **Double-check under lock** (protect against TOCTOU races)
4. **Create and record** (while holding lock)
5. **Activate outside lock** (avoid lock contention during I/O)

---

### 14. MessageBus

**Purpose**: Routes messages across nodes with correlation tracking.

**Structure**:
```rust
pub struct MessageBus<N, T, TP>
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
{
    network: N,
    time: T,
    task_provider: TP,
    node_id: NodeId,
    peers: HashMap<NodeId, Peer<N, T, TP>>,
    pending_requests: RefCell<HashMap<CorrelationId, CallbackData>>,
    next_correlation_id: Cell<u64>,
}
```

**Fields**:
- `network`, `time`, `task_provider` - Provider abstractions
- `node_id` - This node's ID
- `peers` - Connections to other nodes
- `pending_requests` - Outstanding request-response pairs (RefCell for interior mutability)
- `next_correlation_id` - Monotonic counter for correlation IDs (Cell, not Atomic in single-threaded)

**Validation Rules**:
- Request MUST register CallbackData before sending
- Response MUST match pending request correlation ID
- Timeout MUST remove pending request

**Invariants**:
- At most one pending request per correlation ID
- Response completion is exactly-once (via completed flag in CallbackData)

**Single-Threaded Pattern**:
```rust
// Generate next correlation ID (single-threaded, Cell is sufficient)
let correlation_id = CorrelationId(self.next_correlation_id.get());
self.next_correlation_id.set(correlation_id.0 + 1);

// Register pending request
self.pending_requests.borrow_mut().insert(correlation_id, callback_data);
```

---

### 15. CallbackData

**Purpose**: Tracks pending request awaiting response.

**Structure**:
```rust
pub struct CallbackData {
    correlation_id: CorrelationId,
    response_sender: oneshot::Sender<Result<Message>>,
    started_at: Instant,
    timeout: Duration,
    completed: Cell<bool>,
}
```

**Fields**:
- `correlation_id` - Matches request to response
- `response_sender` - Channel to deliver response
- `started_at` - Request send time
- `timeout` - Maximum wait duration
- `completed` - Flag for exactly-once completion (Cell, not Atomic in single-threaded)

**Validation Rules**:
- MUST NOT complete more than once
- Timeout task MUST be spawned on creation
- MUST remove from pending_requests on completion

**Invariants**:
- Completion (success, timeout, or cancellation) happens exactly once
- Response delivery is single-threaded (no race conditions in current_thread runtime)

**Single-Threaded Pattern**:
```rust
// Check and mark completed (single-threaded, no compare-exchange needed)
if callback_data.completed.get() {
    return; // Already completed
}
callback_data.completed.set(true);

// Send response via oneshot channel
callback_data.response_sender.send(Ok(response))?;
```

---

### 16. StateSerializer (Trait)

**Purpose**: Pluggable serialization strategy for actor state persistence.

**Trait Definition**:
```rust
pub trait StateSerializer: Clone {
    fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>, StorageError>;
    fn deserialize<T: DeserializeOwned>(&self, data: &[u8]) -> Result<T, StorageError>;
}
```

**Default Implementation (JSON)**:
```rust
#[derive(Clone, Copy)]
pub struct JsonSerializer;

impl StateSerializer for JsonSerializer {
    fn serialize<T: Serialize>(&self, value: &T) -> Result<Vec<u8>, StorageError> {
        serde_json::to_vec(value).map_err(StorageError::from)
    }

    fn deserialize<T: DeserializeOwned>(&self, data: &[u8]) -> Result<T, StorageError> {
        serde_json::from_slice(data).map_err(StorageError::from)
    }
}
```

**Validation Rules**:
- MUST be Clone (for provider pattern)
- MUST handle serialization errors gracefully
- SHOULD use deterministic format for testing (JSON default)

**Use Cases**:
- **JSON** (default): Human-readable, debuggable, zero configuration
- **Bincode** (future): Compact binary format for performance
- **Protobuf** (future): Schema evolution support

---

### 17. StorageProvider (Trait)

**Purpose**: Abstract interface for actor state persistence with pluggable serializer.

**Trait Definition**:
```rust
#[async_trait(?Send)]
pub trait StorageProvider: Clone {
    /// Load raw state bytes for an actor. Returns None if no state exists.
    ///
    /// **Key**: Derived from ActorId as "namespace::actor_type/key"
    /// **Example**: load_state(ActorId("prod", "BankAccount", "alice"))
    async fn load_state(&self, actor_id: &ActorId) -> Result<Option<Vec<u8>>, StorageError>;

    /// Save raw state bytes for an actor. Overwrites existing state.
    ///
    /// **Atomicity**: Implementation-defined (naive storage may not be atomic)
    /// **Key**: Same as load_state
    async fn save_state(&self, actor_id: &ActorId, data: Vec<u8>) -> Result<(), StorageError>;
}
```

**Validation Rules**:
- Implementations MUST support Clone (for provider pattern)
- Keys MUST use format: `"namespace::actor_type/key"`
- Keys MUST NOT exceed 512 characters total (namespace + actor_type + key + delimiters)
- Reserved delimiters `::` and `/` MUST NOT appear in namespace, actor_type, or key fields
- MUST return None for non-existent state (not error)
- Save MUST overwrite existing state completely
- MUST be single-threaded compatible (`#[async_trait(?Send)]`)

**Invariants**:
- save_state followed by load_state returns same data (unless buggify injects failure)
- Namespace isolation: Cannot load state from different namespace
- Actor type isolation: Keys include actor_type for natural partitioning
- Single activation: No concurrency control needed (actor only active on one node)

**Concurrency Guarantees**:
- **Reads**: Multiple nodes MAY read same storage key during activation race (eventual consistency)
- **Writes**: Only one node writes to storage key (single activation guarantee)
- **Race Scenario**: Node A and Node B both read state during activation, but only winner writes
- **No TOCTOU issues**: Lost activation race triggers deactivation before message processing

**Error Handling**:
- Return Result<T, StorageError> for all operations
- No automatic retry (caller decides strategy)
- Framework handles serialization, storage only deals with bytes

---

### 18. ActorState<T>

**Purpose**: Wrapper for actor persistent state with automatic persistence on mutation.

**Structure**:
```rust
pub struct ActorState<T> {
    data: T,
    storage_handle: Box<dyn StateStorage>,  // Internal framework-managed storage
}

// Internal trait for storage abstraction (not public API)
#[async_trait(?Send)]
trait StateStorage {
    async fn persist(&self, data: &[u8]) -> Result<(), StorageError>;
}
```

**Public API**:
```rust
impl<T: Serialize + DeserializeOwned + Clone> ActorState<T> {
    /// Get immutable reference to state
    pub fn get(&self) -> &T {
        &self.data
    }

    /// Update state and persist immediately
    ///
    /// **Persistence**: Serializes state and saves to storage automatically
    /// **Atomicity**: Depends on storage implementation
    /// **Error Handling**: Returns StorageError if persistence fails
    pub async fn persist(&mut self, new_data: T) -> Result<(), StorageError> {
        // Serialize state
        let bytes = serde_json::to_vec(&new_data)?;

        // Save to storage
        self.storage_handle.persist(&bytes).await?;

        // Update in-memory state only after successful save
        self.data = new_data;

        Ok(())
    }
}
```

**Framework Construction** (internal, not exposed to actor):
```rust
impl<T> ActorState<T> {
    // Used by framework during activation
    pub(crate) fn new_with_storage<S: StateSerializer>(
        data: T,
        actor_id: ActorId,
        storage: Arc<dyn StorageProvider>,
        serializer: S,
    ) -> Self {
        Self {
            data,
            storage_handle: Box::new(ProductionStateStorage {
                actor_id,
                storage,
                serializer,
            }),
        }
    }

    // Used in tests to capture persistence calls
    pub(crate) fn new_with_test_storage(
        data: T,
        saves: Rc<RefCell<Vec<Vec<u8>>>>,
    ) -> Self {
        Self {
            data,
            storage_handle: Box::new(TestStateStorage { saves }),
        }
    }

    // Stateless actor (no persistence)
    pub(crate) fn new_without_storage(data: T) -> Self {
        Self {
            data,
            storage_handle: Box::new(NoOpStorage),
        }
    }
}
```

**Validation Rules**:
- State type `T` MUST implement `Serialize + DeserializeOwned + Clone`
- `persist()` MUST serialize before saving (framework responsibility)
- `persist()` MUST update in-memory state only after successful save
- Actor MUST NOT access `storage_handle` directly

**Invariants**:
- In-memory state matches persisted state after successful `persist()` call
- Failed `persist()` leaves in-memory state unchanged
- `get()` returns immutable reference (prevents mutation without persistence)

**Error Handling**:
- `persist()` returns `StorageError` on serialization failure
- `persist()` returns `StorageError` on storage backend failure
- Actor code must handle storage errors explicitly

**Usage Pattern**:
```rust
pub struct BankAccountActor {
    actor_id: ActorId,
    state: ActorState<BankAccountState>,  // Persistent state
    cache: HashMap<String, String>,        // Ephemeral (not persisted)
}

impl BankAccountActor {
    pub async fn deposit(&mut self, amount: u64) -> Result<u64, ActorError> {
        // Clone current state
        let mut data = self.state.get().clone();

        // Mutate cloned state
        data.balance += amount;

        // Persist (serialize + save + update in-memory)
        self.state.persist(data).await?;

        Ok(self.state.get().balance)
    }
}
```

---

### 19. StorageError

**Purpose**: Error types for storage operations.

**Structure**:
```rust
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// Serialization/deserialization failed
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Underlying I/O error (file, network, etc.)
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Storage system unavailable
    #[error("Storage unavailable")]
    Unavailable,

    /// Key not found (only for operations that require existence)
    #[error("Key not found: {0}")]
    NotFound(String),

    /// Generic operation failure
    #[error("Storage operation failed: {0}")]
    OperationFailed(String),
}
```

**Usage**:
```rust
// In actor method
match storage.save_state(&self.actor_id, data).await {
    Ok(()) => { /* Success */ }
    Err(StorageError::Unavailable) => {
        // Storage down - retry later or fail gracefully
    }
    Err(StorageError::Io(e)) => {
        // Disk full, network error, etc.
    }
    Err(e) => {
        // Other errors
    }
}
```

---

### 18. InMemoryStorage

**Purpose**: In-memory storage implementation for testing and simulation.

**Structure**:
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
        format!("{}::{}/{}", actor_id.namespace, actor_id.actor_type, actor_id.key)
    }
}
```

**Fields**:
- `data` - Shared HashMap via Rc<RefCell<>> (single-threaded)

**Validation Rules**:
- Keys MUST be unique per ActorId
- Data MUST be cloned on load (immutable after return)
- MUST support buggify injection for failure testing

**Invariants**:
- Thread-safe in single-threaded context (RefCell ensures no overlapping borrows)
- Clone creates shallow copy (shared HashMap)
- Deterministic (no async I/O, immediate completion)

**Buggify Integration**:
```rust
#[async_trait(?Send)]
impl StorageProvider for InMemoryStorage {
    async fn save_state(&self, actor_id: &ActorId, data: Vec<u8>) -> Result<(), StorageError> {
        // Inject random failures in simulation
        if buggify_with_prob!(0.1) {
            return Err(StorageError::OperationFailed("Simulated failure".into()));
        }

        let key = Self::storage_key(actor_id);
        self.data.borrow_mut().insert(key, data);
        Ok(())
    }
}
```

---

### 20. Actor Trait (with Typed State)

**Purpose**: Actor lifecycle interface with type-safe state management.

**Trait Definition**:
```rust
#[async_trait(?Send)]
pub trait Actor: Sized {
    /// Persistent state type (default: () for stateless actors)
    type State: Serialize + DeserializeOwned = ();

    /// Get this actor's ID
    fn actor_id(&self) -> &ActorId;

    /// Called when actor is activated (before first message).
    ///
    /// **state**: Previously persisted state loaded by framework (None if first activation)
    /// **Timing**: SetupState stage (Orleans model)
    /// **Failure**: Returns error → actor transitions to Deactivating
    async fn on_activate(&mut self, state: Option<Self::State>) -> Result<(), ActorError>;

    /// Called when actor is deactivated (after last message).
    ///
    /// **Reason**: Why deactivation triggered (idle, shutdown, error)
    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError>;
}
```

**Activation with State Example**:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BankAccountState {
    balance: u64,
}

pub struct BankAccountActor {
    actor_id: ActorId,
    state: ActorState<BankAccountState>,  // Persistent state wrapper
    cache: HashMap<String, String>,        // Ephemeral data (not persisted)
}

#[async_trait(?Send)]
impl Actor for BankAccountActor {
    type State = BankAccountState;  // ✅ Type-safe state

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(&mut self, state: Option<BankAccountState>) -> Result<(), ActorError> {
        // Framework loads state from storage, deserializes, and passes here
        let initial_state = state.unwrap_or(BankAccountState { balance: 0 });

        // Initialize ActorState wrapper (framework provides storage handle internally)
        self.state = ActorState::new_with_storage(
            initial_state,
            self.actor_id.clone(),
            storage,  // Injected by framework
            JsonSerializer,
        );

        tracing::info!("Activated with balance: {}", self.state.get().balance);
        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
        tracing::info!("Deactivating: {:?}, final balance: {}",
                      reason, self.state.get().balance);
        Ok(())
    }
}
```

**Actor Methods with Persistence**:
```rust
impl BankAccountActor {
    pub async fn deposit(&mut self, amount: u64) -> Result<u64, ActorError> {
        // Clone current state
        let mut data = self.state.get().clone();

        // Mutate
        data.balance += amount;

        // Persist (serialize + save + update in-memory)
        self.state.persist(data).await?;

        Ok(self.state.get().balance)
    }
}
```

**Validation Rules**:
- `State` type MUST implement `Serialize + DeserializeOwned`
- Actor MUST handle `None` state (first activation)
- Actor MUST store `ActorState<T>` wrapper, not raw state
- Framework handles all serialization/deserialization
- Actors call `persist()` explicitly when state changes

**Stateless Actor Example**:
```rust
pub struct CalculatorActor {
    actor_id: ActorId,
}

#[async_trait(?Send)]
impl Actor for CalculatorActor {
    // No State type = default () = stateless

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(&mut self, _state: Option<()>) -> Result<(), ActorError> {
        Ok(())  // No state to load
    }

    async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
        Ok(())
    }
}
```

---

## Relationships

### Actor ↔ Storage
- **Zero-to-One**: Actor has at most one StorageProvider
- **Optional**: Not all actors use persistent storage
- Relationship injected at activation time via ActorContext

### Storage ↔ ActorId
- **One-to-Many**: One StorageProvider serves many ActorIds
- **Key-based**: Storage keys derived from ActorId structure
- **Isolation**: Namespace and actor_type provide natural partitioning

### Actor ↔ Node
- **One-to-Many**: One Node hosts many Actors
- **Many-to-One**: One Actor hosted on exactly one Node at a time
- Relationship tracked in `ActorAddress` and `Directory`

### Actor ↔ Message
- **One-to-Many**: One Actor receives/sends many Messages
- **Many-to-One**: One Message targets exactly one Actor
- Relationship defined by `Message.target_actor`

### Message ↔ CorrelationId
- **One-to-One**: One Request has one unique CorrelationId
- **One-to-One**: One Response matches one CorrelationId
- Relationship enforced by `CallbackData` registry

### Directory ↔ ActorCatalog
- **Many-to-Many**: Directory shared across all nodes
- **One-to-One**: Each ActorCatalog references one Directory
- Interaction: Catalog queries/updates Directory during activation

---

## State Transition Diagrams

### Actor Lifecycle
```
[Idle]
  |
  | First message arrives
  ↓
[Creating]
  |
  | Instance created
  ↓
[Activating]
  |
  | on_activate() success → [Valid]
  | on_activate() failure → [Deactivating]
  ↓
[Valid]
  |
  | Process messages sequentially
  | Deactivation triggered (idle timeout, explicit request)
  ↓
[Deactivating]
  |
  | on_deactivate() completes
  ↓
[Invalid]
  |
  | Removed from catalog
  ↓
[Idle]
```

### Request-Response Flow
```
[Client: Create Request]
  |
  | Assign CorrelationId
  ↓
[Client: Register CallbackData]
  |
  | Send via MessageBus
  ↓
[Server: Receive Request]
  |
  | Process message
  ↓
[Server: Send Response]
  |
  | Copy CorrelationId
  ↓
[Client: Receive Response]
  |
  | Match CorrelationId
  | Complete CallbackData
  ↓
[Client: Deliver Result]
```

### Directory Registration
```
[Actor Activation Starts]
  |
  | Catalog.get_or_create()
  ↓
[Check Local Catalog]
  |
  | Not found locally
  ↓
[Lock Catalog]
  |
  | Double-check (race protection)
  ↓
[Register with Directory]
  |
  | directory.register(actor_id, node_id)
  ↓
[Placement Decision]
  |
  | AlreadyRegistered → Forward message
  | PlaceOnNode → Create activation
  | Race → Winner keeps, loser deactivates
  ↓
[Unlock Catalog]
```

---

## Message Flow: End-to-End

This section describes the complete flow from user code calling `actor.call(request)` to receiving the response, including all routing and dispatch mechanisms.

### Flow Overview

```
[User Code]
    │
    │ alice.call(DepositRequest { amount: 100 })
    │
    ↓
[ActorRef::call()]
    │
    │ 1. Serialize request to JSON
    │ 2. Extract method name from type
    │ 3. Create Message with method_name
    │ 4. Assign CorrelationId
    │ 5. Register CallbackData
    │
    ↓
[MessageBus (Sender Node)]
    │
    │ 6. Directory lookup (target_actor → target_node)
    │ 7. Create/reuse Peer connection
    │ 8. Serialize Message to wire format
    │
    ↓
[Network (PeerTransport)]
    │
    │ 9. TCP send to target_node
    │
    ↓
[MessageBus (Receiver Node)]
    │
    │ 10. Receive from Peer
    │ 11. Deserialize wire format → Message
    │ 12. Route to ActorCatalog
    │
    ↓
[ActorCatalog]
    │
    │ 13. get_or_create_activation(actor_id)
    │     • Fast path: existing activation
    │     • Slow path: double-check lock + create
    │ 14. Queue message to actor context
    │
    ↓
[Actor Message Loop]
    │
    │ 15. Dequeue message from actor queue
    │ 16. Check activation state == Valid
    │ 17. Dispatch to handler (see below)
    │
    ↓
[MessageHandler Dispatch]
    │
    │ 18. Match message.method_name
    │ 19. Deserialize payload to request type
    │ 20. Call MessageHandler<Req, Res>::handle()
    │
    ↓
[Actor Method (Business Logic)]
    │
    │ 21. actor.deposit(amount)
    │ 22. Update actor state
    │ 23. Return result
    │
    ↓
[Response Path]
    │
    │ 24. Serialize response to JSON
    │ 25. Create response Message (swap sender/target)
    │ 26. Copy correlation_id from request
    │ 27. Send via MessageBus
    │
    ↓
[Network (PeerTransport)]
    │
    │ 28. TCP send back to sender_node
    │
    ↓
[MessageBus (Original Sender)]
    │
    │ 29. Receive response
    │ 30. Match correlation_id to CallbackData
    │ 31. Complete oneshot channel
    │
    ↓
[ActorRef::call() awaiter]
    │
    │ 32. Deserialize response payload
    │ 33. Return result to user
    │
    ↓
[User Code]
    Result<Res, ActorError>
```

### Step-by-Step Details

#### Sending Side (Steps 1-9)

**Step 1-5: ActorRef::call() prepares message**
```rust
impl<A: Actor> ActorRef<A> {
    pub async fn call<Req, Res>(&self, request: Req) -> Result<Res, ActorError> {
        // 1. Serialize request to JSON
        let payload = serde_json::to_vec(&request)?;

        // 2. Extract method name from type (e.g., "DepositRequest")
        let method_name = std::any::type_name::<Req>()
            .rsplit("::")
            .next()
            .unwrap_or("unknown");

        // 3. Generate correlation ID
        let correlation_id = self.message_bus.next_correlation_id();

        // 4. Create message with routing info
        let message = Message::request(
            correlation_id,
            self.actor_id.clone(),        // target_actor
            self.sender_actor_id.clone(), // sender_actor (system or current actor)
            target_node,                   // filled by MessageBus
            sender_node,                   // filled by MessageBus
            method_name,                   // "DepositRequest"
            payload,                       // JSON bytes
            timeout,                       // 30 seconds default
        );

        // 5. Register callback for response
        let (response_tx, response_rx) = oneshot::channel();
        let callback_data = CallbackData::new(correlation_id, response_tx, timeout);
        self.message_bus.register_callback(correlation_id, callback_data);

        // Continue to MessageBus...
    }
}
```

**Step 6-8: MessageBus routes message**
```rust
impl MessageBus {
    async fn send_request(&self, mut message: Message) -> Result<Message> {
        // 6. Directory lookup
        let target_node = self.directory.lookup(&message.target_actor).await
            .unwrap_or_else(|| {
                // Actor not yet activated - choose placement
                self.directory.choose_node_for_placement()
            });

        // Fill in node addresses
        message.target_node = target_node;
        message.sender_node = self.node_id.clone();

        // 7. Get peer connection to target node
        let peer = self.get_or_create_peer(target_node).await?;

        // 8. Serialize to wire format
        let wire_bytes = ActorEnvelope::serialize(&message)?;

        // 9. Send via TCP
        peer.send(wire_bytes)?;

        // Await response (blocked on CallbackData.response_rx)...
    }
}
```

#### Receiving Side (Steps 10-20)

**Step 10-12: MessageBus receives and routes**
```rust
impl MessageBus {
    async fn receive_loop(&self) {
        loop {
            // 10. Receive from peer
            let wire_bytes = self.peer.receive().await?;

            // 11. Deserialize
            let message = ActorEnvelope::deserialize(&wire_bytes)?;

            // 12. Route based on direction
            match message.direction {
                Direction::Request | Direction::OneWay => {
                    // Route to actor catalog
                    self.handle_incoming_request(message).await?;
                }
                Direction::Response => {
                    // Route to callback
                    self.handle_incoming_response(message).await?;
                }
            }
        }
    }
}
```

**Step 13-17: ActorCatalog activates and queues**
```rust
impl MessageBus {
    async fn handle_incoming_request(&self, message: Message) -> Result<()> {
        // 13. Get or create actor activation
        let context = self.catalog.get_or_create_activation(message.target_actor.clone()).await?;

        // 14. Enqueue message
        context.message_queue.borrow_mut().push_back(message);

        // 15. Spawn message processing task if not already running
        if !context.is_processing.get() {
            context.is_processing.set(true);
            self.task_provider.spawn_task("actor_message_loop", async move {
                self.process_actor_messages(context).await;
            });
        }

        Ok(())
    }

    async fn process_actor_messages(&self, context: Arc<ActorContext>) {
        loop {
            // 16. Dequeue next message
            let message = match context.message_queue.borrow_mut().pop_front() {
                Some(msg) => msg,
                None => break, // Queue empty
            };

            // 17. Check activation state
            if *context.state.borrow() != ActivationState::Valid {
                // Skip processing if not valid
                continue;
            }

            // Dispatch to handler...
        }
    }
}
```

**Step 18-20: Dispatch to handler**
```rust
impl MessageBus {
    async fn dispatch_to_handler(&self, context: &ActorContext, message: Message) -> Result<Vec<u8>> {
        // 18. Match method name to handler
        match message.method_name.as_str() {
            "DepositRequest" => {
                // 19. Deserialize payload
                let request: DepositRequest = serde_json::from_slice(&message.payload)?;

                // 20. Call handler trait
                let mut actor = context.actor_instance.borrow_mut();
                let response = <BankAccountActor as MessageHandler<DepositRequest, u64>>::handle(
                    &mut *actor,
                    request
                ).await?;

                // Serialize response
                Ok(serde_json::to_vec(&response)?)
            }
            "WithdrawRequest" => { /* similar */ }
            "GetBalanceRequest" => { /* similar */ }
            _ => Err(ActorError::UnknownMethod(message.method_name.clone()))
        }
    }
}
```

#### Response Path (Steps 21-33)

**Step 21-27: Create and send response**
```rust
// 21-23. Actor method executes (business logic in user code)
impl MessageHandler<DepositRequest, u64> for BankAccountActor {
    async fn handle(&mut self, req: DepositRequest) -> Result<u64, ActorError> {
        // 21. Call actual actor method
        self.deposit(req.amount).await
        // 22-23. State updated, result returned
    }
}

// 24-27. MessageBus sends response
let response_payload = self.dispatch_to_handler(&context, message).await?;

let response_message = Message::response(&message, response_payload);
// Automatically: swaps sender/target, copies correlation_id

let wire_bytes = ActorEnvelope::serialize(&response_message)?;
self.peer.send(wire_bytes)?;
```

**Step 28-33: Response delivered to caller**
```rust
impl MessageBus {
    async fn handle_incoming_response(&self, response: Message) -> Result<()> {
        // 29-30. Match correlation ID
        let callback_data = self.pending_requests
            .borrow_mut()
            .remove(&response.correlation_id)
            .ok_or(ActorError::UnknownCorrelationId)?;

        // 31. Complete oneshot channel
        callback_data.response_sender.send(Ok(response))?;

        Ok(())
    }
}

// Back in ActorRef::call()...
// 32. Await response_rx
let response = response_rx.await??;

// 33. Deserialize and return
let result: Res = serde_json::from_slice(&response.payload)?;
Ok(result)
```

### Forwarding Flow (Stale Cache)

When directory cache is stale, messages are forwarded:

```
[Node A: Cached "alice" → Node B]
    │
    │ Send message to Node B
    ↓
[Node B: Actor not found]
    │
    │ Lookup in directory → Node C
    │ message.forward_count += 1
    │ Add cache_invalidation header
    ↓
[Node C: Process message]
    │
    │ Handle request normally
    │ Create response with cache_invalidation
    ↓
[Node A: Receive response]
    │
    │ Process cache_invalidation
    │ Update local cache: "alice" → Node C
    │ Future messages go directly to Node C
```

**Forward count protection**:
- MAX_FORWARD_COUNT = 2
- If message.forward_count > 2, reject with `ActorError::TooManyForwards`
- Prevents infinite loops in pathological cases

### Method Dispatch Strategies

**Current Design: Type Name + Match Statement**
```rust
let method_name = std::any::type_name::<Req>(); // "DepositRequest"

match method_name {
    "DepositRequest" => call_handler::<DepositRequest, u64>(...),
    "WithdrawRequest" => call_handler::<WithdrawRequest, u64>(...),
    ...
}
```

**Alternative: Handler Registry (Future)**
```rust
pub struct HandlerRegistry {
    handlers: HashMap<String, Box<dyn Fn(ActorContext, Vec<u8>) -> Pin<Box<dyn Future>>>>,
}

// Registration
registry.register("DepositRequest", |ctx, payload| Box::pin(async move {
    let req: DepositRequest = serde_json::from_slice(&payload)?;
    ctx.actor.deposit(req.amount).await
}));

// Dispatch
let handler = registry.get(&message.method_name)?;
handler(context, message.payload).await?;
```

Benefits:
- No monomorphization per actor type
- Runtime-extensible actor types
- Smaller binary size

Trade-offs:
- Less type-safe
- Runtime overhead (HashMap lookup + dynamic dispatch)
- Requires registration boilerplate

**Decision**: Start with match statement (simpler, type-safe), optimize to registry if needed.

---

## Validation Rules Summary

### Message Validation
1. Request MUST have timeout
2. Response MUST match pending correlation ID
3. OneWay MUST NOT expect response
4. Target actor MUST be valid ActorId
5. Payload MUST be valid JSON

### State Validation
1. Actor MUST be Valid to process messages
2. State transitions MUST follow lifecycle rules
3. Concurrent state changes MUST be serialized via Mutex

### Directory Validation
1. ActorId maps to at most one NodeId
2. Registration idempotent for same pair
3. Unregister only if currently registered
4. Cache timestamps monotonically increasing

### Concurrency Validation
1. Message queue processed sequentially per actor
2. CallbackData completed exactly once
3. Directory updates atomic within operation

---

## Invariants

### Global Invariants
1. **Single Activation**: Each ActorId corresponds to at most one active ActorContext across all nodes
2. **Sequential Processing**: Messages for same actor processed one-at-a-time
3. **Correlation Uniqueness**: Each pending request has unique CorrelationId per node
4. **Eventual Consistency**: All nodes converge to same actor location after stabilization

### Local Invariants (per node)
1. **Catalog Consistency**: Actor in activations map ⟺ state != Invalid
2. **Queue Ordering**: Messages dequeued in FIFO order
3. **Timeout Accuracy**: Timeouts trigger within configured window
4. **State Machine**: Actor state transitions follow ActivationState rules

### Testing Invariants (for validation)
1. **Banking Invariant**: Sum of all balances constant across operations
2. **Message Delivery**: 100% delivery in static cluster (no drops)
3. **No Deadlocks**: No message loops causing permanent hangs

---

## Example Scenarios

### Scenario 1: First Message to Actor
1. Client sends message to "alice" (not yet active)
2. MessageBus routes to Directory
3. Directory placement algorithm chooses node_2
4. Node_2 ActorCatalog creates ActorContext
5. Actor transitions: Creating → Activating → Valid
6. Message processed, response sent
7. Response correlated and delivered to client

### Scenario 2: Concurrent Activation Race
1. Node_1 and Node_2 simultaneously receive messages for "bob"
2. Both attempt directory.register("bob", their_node_id)
3. Directory serializes: Node_1's registration wins
4. Node_1 continues activation → Valid
5. Node_2 receives AlreadyRegistered(Node_1)
6. Node_2 forwards message to Node_1
7. Node_2 deactivates its partial activation

### Scenario 3: Actor Deactivation
1. Actor "charlie" idle for 10 minutes
2. Idle detector triggers deactivation
3. Actor transitions: Valid → Deactivating
4. on_deactivate() runs (cleanup)
5. Directory.unregister("charlie")
6. ActorCatalog removes from activations map
7. Actor transitions: Deactivating → Invalid

---

## Future Extensions (Out of Scope)

- **Persistence**: ActorContext includes state storage
- **Migration**: ActorAddress includes migration metadata
- **Transactions**: Message includes transaction context
- **Reentrancy**: ActorContext includes concurrent message tracking

---

**Files to Implement**:
- `moonpool/src/actor/id.rs` - ActorId, NodeId, CorrelationId
- `moonpool/src/messaging/message.rs` - Message, Direction, MessageFlags
- `moonpool/src/messaging/address.rs` - ActorAddress
- `moonpool/src/actor/lifecycle.rs` - ActivationState
- `moonpool/src/actor/context.rs` - ActorContext (with optional storage)
- `moonpool/src/actor/traits.rs` - Actor trait (with storage parameter)
- `moonpool/src/directory/traits.rs` - Directory trait
- `moonpool/src/directory/simple.rs` - SimpleDirectory
- `moonpool/src/actor/catalog.rs` - ActorCatalog
- `moonpool/src/messaging/bus.rs` - MessageBus
- `moonpool/src/messaging/correlation.rs` - CallbackData
- `moonpool/src/storage/traits.rs` - StorageProvider trait
- `moonpool/src/storage/error.rs` - StorageError
- `moonpool/src/storage/memory.rs` - InMemoryStorage
