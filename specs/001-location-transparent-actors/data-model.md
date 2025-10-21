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
- Namespace is set at ActorSystem bootstrap (cluster-wide configuration)
- All actors within a cluster share the same namespace
- Cannot send messages across namespaces (namespace boundary = cluster boundary)
- ActorSystem automatically applies namespace to all ActorIds it creates

**Examples**:
```rust
// ActorSystem bootstrapped with namespace "prod"
let system = ActorSystem::new("prod").await?;

// get_actor() automatically applies "prod" namespace
let alice = system.get_actor("BankAccount", "alice");
// Internally creates: ActorId { namespace: "prod", actor_type: "BankAccount", key: "alice" }
// String format: "prod::BankAccount/alice"

// Different cluster with different namespace
let staging_system = ActorSystem::new("staging").await?;
let alice_staging = staging_system.get_actor("BankAccount", "alice");
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

**Purpose**: Identifies a physical node in the cluster.

**Structure**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(u64);
```

**Fields**:
- `0: u64` - Numeric node identifier (0-based index or assigned ID)

**Validation Rules**:
- MUST be unique within the cluster
- MUST remain stable during node lifetime

**Invariants**:
- NodeId equality implies same physical node
- NodeId persists across actor activations/deactivations

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

**Validation Rules**:
- Request direction MUST have `time_to_expiry`
- Response correlation_id MUST match pending request
- OneWay MUST NOT have response expectation

**Invariants**:
- Response swaps target/sender from request
- Response inherits correlation_id from request

---

### 5. Direction

**Purpose**: Indicates message flow semantics.

**Structure**:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Request,   // Expects response
    Response,  // Reply to request
    OneWay,    // Fire-and-forget
}
```

**State Transitions**:
```
Request → Response  (matching correlation_id)
OneWay → (terminal, no response)
```

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
- Transitions guarded by `Mutex<ActivationState>`

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
    pub state: Mutex<ActivationState>,
    pub message_queue: Mutex<VecDeque<Message>>,
    pub actor_instance: Mutex<A>,
    pub activation_time: Instant,
    pub last_message_time: Mutex<Instant>,
}
```

**Fields**:
- `actor_id` - Actor's unique identifier
- `node_id` - Hosting node
- `state` - Current lifecycle state (guarded by Mutex)
- `message_queue` - Unbounded FIFO queue for incoming messages
- `actor_instance` - User-defined actor implementation
- `activation_time` - When actor was activated
- `last_message_time` - For idle timeout detection

**Validation Rules**:
- message_queue can only be dequeued when state == Valid
- actor_instance MUST NOT be accessed outside message processing
- state transitions MUST follow ActivationState rules

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

### 12. ActorCatalog

**Purpose**: Local registry of actors on a single node.

**Structure**:
```rust
pub struct ActorCatalog {
    activations: Mutex<HashMap<ActorId, Arc<ActorContext<A>>>>,
    node_id: NodeId,
    directory: Arc<dyn Directory>,
}
```

**Fields**:
- `activations` - Map of active actors on this node
- `node_id` - This node's identifier
- `directory` - Reference to distributed directory

**Validation Rules**:
- Actor MUST be in activations map if state != Invalid
- Actor MUST NOT be in map if deactivation completed

**Invariants**:
- Double-check locking prevents duplicate activations
- Activation state transitions thread-safe via Mutex

---

### 13. MessageBus

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
    pending_requests: Mutex<HashMap<CorrelationId, CallbackData>>,
    next_correlation_id: AtomicU64,
}
```

**Fields**:
- `network`, `time`, `task_provider` - Provider abstractions
- `node_id` - This node's ID
- `peers` - Connections to other nodes
- `pending_requests` - Outstanding request-response pairs
- `next_correlation_id` - Monotonic counter for correlation IDs

**Validation Rules**:
- Request MUST register CallbackData before sending
- Response MUST match pending request correlation ID
- Timeout MUST remove pending request

**Invariants**:
- At most one pending request per correlation ID
- Response completion is exactly-once (via AtomicBool)

---

### 14. CallbackData

**Purpose**: Tracks pending request awaiting response.

**Structure**:
```rust
pub struct CallbackData {
    correlation_id: CorrelationId,
    response_sender: oneshot::Sender<Result<Message>>,
    started_at: Instant,
    timeout: Duration,
    completed: AtomicBool,
}
```

**Fields**:
- `correlation_id` - Matches request to response
- `response_sender` - Channel to deliver response
- `started_at` - Request send time
- `timeout` - Maximum wait duration
- `completed` - Atomic flag for exactly-once completion

**Validation Rules**:
- MUST NOT complete more than once
- Timeout task MUST be spawned on creation
- MUST remove from pending_requests on completion

**Invariants**:
- Completion (success, timeout, or cancellation) happens exactly once
- Response delivery is atomic via compare-exchange

---

## Relationships

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
- `moonpool/src/actor/context.rs` - ActorContext
- `moonpool/src/directory/traits.rs` - Directory trait
- `moonpool/src/directory/simple.rs` - SimpleDirectory
- `moonpool/src/actor/catalog.rs` - ActorCatalog
- `moonpool/src/messaging/bus.rs` - MessageBus
- `moonpool/src/messaging/correlation.rs` - CallbackData
