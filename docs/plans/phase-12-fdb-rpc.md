# Phase 12: FDB-Style Static Messaging

## Overview

Refactor moonpool to align with FoundationDB's RPC model with clean separation between transport primitives and messaging layer. This phase focuses on **static messaging** (fixed endpoints), leaving room for future **virtual actors** (Orleans-style grains).

### References
- `docs/analysis/fdb-network.md` - Network simulation implementation guide
- `docs/analysis/fdb-network-sim.md` - Sim2 deterministic simulation
- `docs/analysis/fdb-actor.md` - Actor model and endpoint messaging
- `docs/analysis/flow.md` - Flow language patterns

### Scope

**Phase 12 (This Phase)**:
- Foundation: Raw transport primitives (token + bytes)
- Moonpool: Static messaging (RequestStream, ReplyPromise, EndpointMap)

**Phase 13 (Future)**:
- Built-in receivers (Ping, EndpointNotFound)
- Dynamic endpoint registration (ServerDBInfo pattern)

**Future Phases**:
- Virtual actors (Orleans-style grains with activation/deactivation)

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  moonpool                                                        │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │ messaging/                                                   ││
│  │ ├── static/           # Phase 12: FDB-style fixed endpoints ││
│  │ │   ├── EndpointMap, FlowTransport                          ││
│  │ │   ├── RequestStream<T>, ReplyPromise<T>                   ││
│  │ │   ├── NetNotifiedQueue<T>, networkSender                  ││
│  │ │   └── Interface macro                                     ││
│  │ │                                                            ││
│  │ └── virtual/          # Future: Orleans-style grains        ││
│  │     └── (placeholder)                                       ││
│  └─────────────────────────────────────────────────────────────┘│
│                              │ uses                              │
├──────────────────────────────┼──────────────────────────────────┤
│  moonpool-foundation (Simulation + Transport Primitives)         │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │ SIMULATION CORE                                              ││
│  │ SimWorld, EventQueue, assertions, buggify, invariants       ││
│  │ TimeProvider, TaskProvider, NetworkProvider, RandomProvider ││
│  ├─────────────────────────────────────────────────────────────┤│
│  │ TRANSPORT PRIMITIVES                                         ││
│  │ UID (128-bit), Endpoint, NetworkAddress                     ││
│  │ Wire: [len:4][crc32c:4][token:16][payload]                  ││
│  │ Peer (connection + auto-reconnect)                          ││
│  │ send_reliable(token, bytes), send_unreliable(token, bytes)  ││
│  │ async fn receive() → (UID, Vec<u8>)                         ││
│  │ WellKnownToken enum (constants only)                        ││
│  └─────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
```

## Moonpool Module Structure

```
moonpool/src/
├── lib.rs
├── error.rs
└── messaging/
    ├── mod.rs
    ├── static/                    # Phase 12: FDB-style static endpoints
    │   ├── mod.rs
    │   ├── endpoint_map.rs        # Token → receiver routing
    │   ├── flow_transport.rs      # Coordinator (peers + dispatch)
    │   ├── net_notified_queue.rs  # Typed message queue
    │   ├── request_stream.rs      # RequestStream<T>
    │   ├── reply_promise.rs       # ReplyPromise<T> + networkSender
    │   └── interface.rs           # Interface macro
    │
    └── virtual/                   # Future: Orleans-style virtual actors
        └── mod.rs                 # Placeholder
```

### Design Philosophy

**Static Messaging** (FDB-style):
- Endpoints are fixed at compile time or registration time
- Type `T` is baked into `NetNotifiedQueue<T>` at compile time
- Well-known endpoints use O(1) array lookup
- Explicit endpoint management

**Virtual Actors** (Orleans-style, future):
- Actors are addressed by identity (grain ID), not endpoint
- Runtime activates/deactivates actors on demand
- Location-transparent - runtime decides where to place
- Automatic lifecycle management

## Design Constraints

### Single-Threaded Runtime
Moonpool runs on a single-threaded Tokio runtime (`LocalRuntime`). This affects the design:

- Use `Rc<RefCell<>>` instead of `Arc<RwLock<>>` - no synchronization overhead
- Use `#[async_trait(?Send)]` for all async traits
- No `Send`/`Sync` bounds required on types
- All spawned tasks via `spawn_local`, never `spawn`

### Rust Async Patterns
Follow idiomatic Rust async:

```rust
// Foundation: Simple async fn for Peer receive
impl Peer {
    pub async fn receive(&mut self) -> Result<(UID, Vec<u8>), PeerError>;
}

// Moonpool: FutureStream for RequestStream
impl<T> Stream for FutureStream<T> {
    type Item = T;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>>;
}
```

### No Unwrap Policy
Per CLAUDE.md, all code must use `Result<T, E>` with `?` - no `unwrap()` calls.

## Wire Format (FDB-Compatible)

```
┌────────────────────────────────────────────────────────────────┐
│  uint32_t length        │ Total packet size including header   │
├────────────────────────────────────────────────────────────────┤
│  uint32_t checksum      │ CRC32C of (token + payload)          │
├────────────────────────────────────────────────────────────────┤
│  UID token (16 bytes)   │ Destination endpoint identifier      │
│  ├── first: u64         │                                      │
│  └── second: u64        │                                      │
├────────────────────────────────────────────────────────────────┤
│  payload (N bytes)      │ Custom serde (current approach)      │
└────────────────────────────────────────────────────────────────┘
```

---

# PHASE 1: Foundation Cleanup

## 1.1 Define Core Types

**File**: `moonpool-foundation/src/types.rs` (NEW)

```rust
/// 128-bit unique identifier (FDB-compatible)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UID {
    pub first: u64,
    pub second: u64,
}

impl UID {
    pub fn random() -> Self; // From deterministic RNG
    pub fn well_known(token_id: u32) -> Self {
        UID { first: u64::MAX, second: token_id as u64 }  // FDB pattern: first = -1
    }
    pub fn is_well_known(&self) -> bool {
        self.first == u64::MAX
    }
}

/// Network address (IPv4/IPv6 + port)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetworkAddress {
    pub ip: IpAddr,
    pub port: u16,
    pub flags: u16,  // FLAG_TLS, FLAG_PUBLIC
}

/// List of addresses (primary + optional secondary)
#[derive(Debug, Clone)]
pub struct NetworkAddressList {
    pub primary: NetworkAddress,
    pub secondary: Option<NetworkAddress>,
}

/// Endpoint = Address + Token
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub addresses: NetworkAddressList,
    pub token: UID,
}

impl Endpoint {
    pub fn well_known(addresses: NetworkAddressList, token_id: u32) -> Self {
        Endpoint { addresses, token: UID::well_known(token_id) }
    }
}
```

## 1.2 Define Well-Known Token Constants

**File**: `moonpool-foundation/src/well_known.rs` (NEW)

```rust
/// Well-known endpoint tokens (FDB-compatible pattern)
/// These are compile-time constants, receivers are in moonpool crate
#[repr(u32)]
pub enum WellKnownToken {
    EndpointNotFound = 0,
    Ping = 1,
    UnauthorizedEndpoint = 2,
    // Reserved 3-63 for future system use
}

pub const WELL_KNOWN_RESERVED_COUNT: usize = 64;
```

## 1.3 Implement Wire Format

**File**: `moonpool-foundation/src/wire.rs` (NEW)

```rust
use crc32c::crc32c;

pub const HEADER_SIZE: usize = 4 + 4 + 16;  // length + checksum + token

#[derive(Debug, Clone)]
pub struct PacketHeader {
    pub length: u32,
    pub checksum: u32,
    pub token: UID,
}

impl PacketHeader {
    pub fn serialize(&self, buf: &mut [u8]);
    pub fn deserialize(buf: &[u8]) -> Result<Self, WireError>;
}

pub fn serialize_packet(token: UID, payload: &[u8]) -> Vec<u8> {
    let length = (HEADER_SIZE + payload.len()) as u32;
    let mut data = Vec::with_capacity(length as usize);

    // Compute checksum over token + payload
    let mut checksum_data = Vec::with_capacity(16 + payload.len());
    checksum_data.extend_from_slice(&token.first.to_le_bytes());
    checksum_data.extend_from_slice(&token.second.to_le_bytes());
    checksum_data.extend_from_slice(payload);
    let checksum = crc32c(&checksum_data);

    // Write header
    data.extend_from_slice(&length.to_le_bytes());
    data.extend_from_slice(&checksum.to_le_bytes());
    data.extend_from_slice(&token.first.to_le_bytes());
    data.extend_from_slice(&token.second.to_le_bytes());
    data.extend_from_slice(payload);

    data
}

pub fn deserialize_packet(data: &[u8]) -> Result<(UID, Vec<u8>), WireError> {
    // Validate length, checksum, extract token + payload
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("insufficient data: need {needed}, have {have}")]
    InsufficientData { needed: usize, have: usize },
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: u32, actual: u32 },
    #[error("packet too large: {size} bytes")]
    PacketTooLarge { size: usize },
}
```

## 1.4 Refactor Peer for Raw Transport

**File**: `moonpool-foundation/src/network/peer/core.rs` (MODIFY)

Strip down to raw (token, bytes) interface:

```rust
impl<N, T, TP> Peer<N, T, TP> {
    /// Send packet reliably (queued, will retry on reconnect)
    pub async fn send_reliable(&self, token: UID, payload: &[u8]) -> Result<(), PeerError>;

    /// Send packet unreliably (best-effort, dropped if not connected)
    pub fn send_unreliable(&self, token: UID, payload: &[u8]) -> Result<(), PeerError>;

    /// Receive next packet (blocks until available)
    pub async fn receive(&self) -> Result<(UID, Vec<u8>), PeerError>;

    /// Try to receive without blocking
    pub fn try_receive(&self) -> Option<(UID, Vec<u8>)>;
}
```

## 1.5 Files to DELETE from Foundation

Remove all high-level transport code:

```
moonpool-foundation/src/network/transport/
├── client.rs          # DELETE - ClientTransport
├── server.rs          # DELETE - ServerTransport
├── driver.rs          # DELETE - TransportDriver
├── envelope.rs        # DELETE - Envelope traits
├── protocol.rs        # DELETE - TransportProtocol
├── request_response_envelope.rs  # DELETE
├── types.rs           # DELETE - TransportError, etc.
└── mod.rs             # DELETE
```

## 1.6 Files to KEEP in Foundation

```
moonpool-foundation/src/
├── lib.rs                 # MODIFY - update exports
├── sim.rs                 # KEEP - SimWorld
├── runner.rs              # KEEP - SimulationBuilder
├── events.rs              # KEEP - EventQueue
├── assertions.rs          # KEEP
├── buggify.rs             # KEEP
├── invariants.rs          # KEEP
├── state_registry.rs      # KEEP
├── error.rs               # KEEP
├── rng.rs                 # KEEP
├── sleep.rs               # KEEP
├── network_state.rs       # KEEP
├── tokio_runner.rs        # KEEP
│
├── types.rs               # NEW - UID, Endpoint, NetworkAddress
├── well_known.rs          # NEW - WellKnownToken enum
├── wire.rs                # NEW - Wire format serialization
│
├── network/
│   ├── mod.rs             # MODIFY
│   ├── traits.rs          # KEEP - NetworkProvider, TcpListenerTrait
│   ├── config.rs          # KEEP - NetworkConfiguration
│   ├── peer/
│   │   ├── mod.rs         # KEEP
│   │   ├── core.rs        # MODIFY - raw (token, bytes) API
│   │   ├── config.rs      # KEEP
│   │   ├── metrics.rs     # KEEP
│   │   └── error.rs       # KEEP
│   ├── sim/               # KEEP all
│   └── tokio/             # KEEP all
│
├── time/                  # KEEP all
├── task/                  # KEEP all
└── random/                # KEEP all
```

## 1.7 Update Foundation Exports

**File**: `moonpool-foundation/src/lib.rs` (MODIFY)

```rust
// Core types
pub use types::{UID, Endpoint, NetworkAddress, NetworkAddressList};
pub use well_known::{WellKnownToken, WELL_KNOWN_RESERVED_COUNT};
pub use wire::{PacketHeader, serialize_packet, deserialize_packet, WireError, HEADER_SIZE};

// Simulation (unchanged)
pub use sim::{SimWorld, WeakSimWorld};
pub use runner::{SimulationBuilder, SimulationMetrics, SimulationReport, WorkloadTopology};
// ... rest unchanged

// Network primitives
pub use network::{NetworkProvider, TcpListenerTrait};
pub use network::peer::{Peer, PeerConfig, PeerError, PeerMetrics};
pub use network::sim::SimNetworkProvider;
pub use network::tokio::TokioNetworkProvider;
pub use network::config::NetworkConfiguration;

// Providers (unchanged)
pub use time::{TimeProvider, SimTimeProvider, TokioTimeProvider};
pub use task::{TaskProvider, TokioTaskProvider};
pub use random::{RandomProvider, SimRandomProvider};
```

---

# PHASE 2: Moonpool RPC Layer

## 2.1 EndpointMap Implementation

**File**: `moonpool/src/messaging/static/endpoint_map.rs` (NEW)

```rust
use moonpool_foundation::{UID, WELL_KNOWN_RESERVED_COUNT};
use std::collections::HashMap;
use std::sync::Arc;

/// Trait for receiving deserialized messages
pub trait MessageReceiver: Send + Sync {
    fn receive(&self, payload: &[u8]);
    fn is_stream(&self) -> bool { true }
}

/// Maps endpoint tokens to message receivers
pub struct EndpointMap {
    /// Well-known endpoints (O(1) lookup by index)
    well_known: [Option<Arc<dyn MessageReceiver>>; WELL_KNOWN_RESERVED_COUNT],

    /// Dynamic endpoints (hash lookup)
    dynamic: HashMap<UID, Arc<dyn MessageReceiver>>,

    /// Free list for dynamic allocation
    next_index: u32,
}

impl EndpointMap {
    pub fn new() -> Self;

    /// Register well-known endpoint (static, compile-time tokens)
    pub fn insert_well_known(&mut self, token: WellKnownToken, receiver: Arc<dyn MessageReceiver>);

    /// Register dynamic endpoint (runtime allocation)
    pub fn insert(&mut self, receiver: Arc<dyn MessageReceiver>) -> UID;

    /// Lookup receiver by token
    pub fn get(&self, token: &UID) -> Option<Arc<dyn MessageReceiver>>;

    /// Remove dynamic endpoint
    pub fn remove(&mut self, token: &UID);
}
```

## 2.2 FlowTransport Coordinator

**File**: `moonpool/src/messaging/static/flow_transport.rs` (NEW)

```rust
use moonpool_foundation::{
    UID, Endpoint, NetworkAddress, Peer, NetworkProvider, TimeProvider, TaskProvider,
    serialize_packet, deserialize_packet,
};

/// Central transport coordinator (FDB FlowTransport equivalent)
pub struct FlowTransport<N, T, TP> {
    endpoints: EndpointMap,
    peers: HashMap<NetworkAddress, Peer<N, T, TP>>,
    local_addresses: NetworkAddressList,
    network: N,
    time: T,
    task_provider: TP,
}

impl<N, T, TP> FlowTransport<N, T, TP>
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
{
    pub fn new(local_addresses: NetworkAddressList, network: N, time: T, task: TP) -> Self;

    /// Send message unreliably (best-effort)
    pub fn send_unreliable(&self, endpoint: &Endpoint, payload: &[u8]) -> Result<(), TransportError>;

    /// Get or create peer connection
    pub fn get_or_open_peer(&mut self, addr: &NetworkAddress) -> &Peer<N, T, TP>;

    /// Register well-known endpoint
    pub fn add_well_known_endpoint(&mut self, token: WellKnownToken, receiver: Arc<dyn MessageReceiver>);

    /// Register dynamic endpoint
    pub fn add_endpoint(&mut self, receiver: Arc<dyn MessageReceiver>) -> Endpoint;

    /// Process incoming packets and dispatch to receivers
    pub async fn run(&mut self) -> Result<(), TransportError>;
}
```

## 2.3 NetNotifiedQueue (Typed Receiver)

**File**: `moonpool/src/messaging/static/net_notified_queue.rs` (NEW)

```rust
/// Typed message queue that deserializes into T
pub struct NetNotifiedQueue<T> {
    queue: VecDeque<T>,
    endpoint: Endpoint,
    wakers: Vec<Waker>,
}

impl<T: DeserializeOwned> MessageReceiver for NetNotifiedQueue<T> {
    fn receive(&self, payload: &[u8]) {
        let message: T = deserialize(payload).expect("valid message");
        self.queue.push_back(message);
        // Wake waiters
    }
}

impl<T> NetNotifiedQueue<T> {
    pub fn endpoint(&self) -> &Endpoint;
    pub async fn recv(&self) -> T;
    pub fn try_recv(&self) -> Option<T>;
}
```

## 2.4 RequestStream

**File**: `moonpool/src/messaging/static/request_stream.rs` (NEW)

```rust
/// Type-safe request stream (FDB RequestStream equivalent)
pub struct RequestStream<T> {
    queue: Arc<NetNotifiedQueue<T>>,
}

impl<T: DeserializeOwned> RequestStream<T> {
    pub fn new() -> Self;

    pub fn endpoint(&self) -> Endpoint;

    pub fn get_future(&self) -> FutureStream<T>;

    /// Make this a well-known endpoint
    pub fn make_well_known_endpoint(&mut self, transport: &mut FlowTransport, token: WellKnownToken);
}

/// Stream of incoming messages
pub struct FutureStream<T> {
    queue: Arc<NetNotifiedQueue<T>>,
}

impl<T> Stream for FutureStream<T> {
    type Item = T;
    fn poll_next(...) -> Poll<Option<T>>;
}
```

## 2.5 ReplyPromise with networkSender Magic

**File**: `moonpool/src/messaging/static/reply_promise.rs` (NEW)

```rust
/// Promise that resolves across network boundary
pub struct ReplyPromise<T> {
    endpoint: Endpoint,  // Where to send the reply
    _phantom: PhantomData<T>,
}

impl<T: Serialize> ReplyPromise<T> {
    pub fn send(self, value: T) {
        // Serialize and send to endpoint
        let payload = serialize(&value);
        FlowTransport::current().send_unreliable(&self.endpoint, &payload);
    }

    pub fn send_error(self, error: Error) {
        // Send error response
    }
}

/// Serialization trait for ReplyPromise
/// On serialize: just write endpoint token
/// On deserialize: spawn networkSender actor
impl<T: Serialize + DeserializeOwned> Serialize for ReplyPromise<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Only serialize the token
        self.endpoint.token.serialize(serializer)
    }
}

impl<T: Serialize + DeserializeOwned> Deserialize for ReplyPromise<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> {
        let token = UID::deserialize(deserializer)?;
        let endpoint = FlowTransport::current().loaded_endpoint(token);

        // THE MAGIC: spawn networkSender that will send reply
        let (tx, rx) = oneshot::channel();
        spawn_network_sender::<T>(rx, endpoint.clone());

        Ok(ReplyPromise { endpoint, sender: tx })
    }
}

/// Actor that waits for promise fulfillment and sends reply over network
async fn network_sender<T: Serialize>(
    receiver: oneshot::Receiver<Result<T, Error>>,
    endpoint: Endpoint,
) {
    match receiver.await {
        Ok(Ok(value)) => {
            let payload = serialize(&ErrorOr::Ok(value));
            FlowTransport::current().send_unreliable(&endpoint, &payload);
        }
        Ok(Err(e)) => {
            let payload = serialize(&ErrorOr::Err(e));
            FlowTransport::current().send_unreliable(&endpoint, &payload);
        }
        Err(_) => {
            // Promise dropped without fulfillment - send broken_promise error
            let payload = serialize(&ErrorOr::Err(Error::BrokenPromise));
            FlowTransport::current().send_unreliable(&endpoint, &payload);
        }
    }
}
```

## 2.6 Interface Pattern

**File**: `moonpool/src/messaging/static/interface.rs` (NEW)

```rust
/// Macro for defining RPC interfaces (FDB pattern)
#[macro_export]
macro_rules! define_interface {
    (
        $name:ident {
            $($method:ident: $req_type:ty => $resp_type:ty),* $(,)?
        }
    ) => {
        pub struct $name {
            $(pub $method: RequestStream<$req_type>,)*
        }

        impl $name {
            /// Client constructor - create endpoints pointing to remote
            pub fn remote(addr: NetworkAddress) -> Self {
                let base_token = UID::random();
                Self {
                    $($method: RequestStream::with_endpoint(
                        Endpoint::new(addr.clone(), base_token.adjusted($method_index))
                    ),)*
                }
            }

            /// Server constructor - register local endpoints
            pub fn local(transport: &mut FlowTransport) -> Self {
                Self {
                    $($method: {
                        let stream = RequestStream::new();
                        transport.add_endpoint(stream.receiver());
                        stream
                    },)*
                }
            }
        }
    };
}

// Example usage:
define_interface! {
    StorageServerInterface {
        get_value: GetValueRequest => GetValueResponse,
        get_key: GetKeyRequest => GetKeyResponse,
        watch_value: WatchValueRequest => WatchValueResponse,
    }
}
```

---

# PHASE 3: Testing Strategy

## 3.1 Unit Tests (Foundation)

**File**: `moonpool-foundation/src/wire.rs` tests

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_serialize_deserialize_packet() {
        let token = UID::random();
        let payload = b"hello world";
        let packet = serialize_packet(token, payload);
        let (recv_token, recv_payload) = deserialize_packet(&packet).unwrap();
        assert_eq!(token, recv_token);
        assert_eq!(payload, &recv_payload[..]);
    }

    #[test]
    fn test_checksum_validation() {
        let packet = serialize_packet(UID::random(), b"test");
        let mut corrupted = packet.clone();
        corrupted[HEADER_SIZE] ^= 0xFF;  // Flip a payload bit
        assert!(matches!(deserialize_packet(&corrupted), Err(WireError::ChecksumMismatch { .. })));
    }

    #[test]
    fn test_well_known_token() {
        let token = UID::well_known(WellKnownToken::Ping as u32);
        assert!(token.is_well_known());
        assert_eq!(token.first, u64::MAX);
        assert_eq!(token.second, 1);
    }
}
```

## 3.2 Peer Tests (Foundation)

```rust
#[cfg(test)]
mod peer_tests {
    #[tokio::test]
    async fn test_peer_send_receive() {
        // Test raw (token, bytes) send/receive
    }

    #[tokio::test]
    async fn test_peer_reconnection() {
        // Test auto-reconnect with exponential backoff
    }

    #[tokio::test]
    async fn test_send_unreliable_when_disconnected() {
        // Best-effort drops when not connected
    }
}
```

## 3.3 EndpointMap Tests (Moonpool)

```rust
#[cfg(test)]
mod endpoint_map_tests {
    #[test]
    fn test_well_known_registration() {
        let mut map = EndpointMap::new();
        let receiver = Arc::new(MockReceiver::new());
        map.insert_well_known(WellKnownToken::Ping, receiver.clone());

        let found = map.get(&UID::well_known(WellKnownToken::Ping as u32));
        assert!(found.is_some());
    }

    #[test]
    fn test_dynamic_registration() {
        let mut map = EndpointMap::new();
        let receiver = Arc::new(MockReceiver::new());
        let token = map.insert(receiver);

        assert!(!token.is_well_known());
        assert!(map.get(&token).is_some());
    }

    #[test]
    fn test_o1_lookup() {
        // Verify well-known is O(1) array access, not hash lookup
    }
}
```

## 3.4 Simulation Tests (FDB-Inspired)

**Reference**: FDB's `Ping.actor.cpp`, `ClogSingleConnection.actor.cpp`

```rust
// Test: Basic ping-pong with typed messages
#[test]
fn test_ping_pong_simulation() {
    SimulationBuilder::new()
        .with_seed_range(0..1000)
        .with_workload(|network, time, task, random, topology| async move {
            // Server registers PingReceiver
            // Client sends PingRequest, expects PingResponse
            // Verify message delivery
        })
        .run_until_all_sometimes_reached();
}

// Test: Connection failure and recovery
#[test]
fn test_connection_failure_recovery() {
    SimulationBuilder::new()
        .with_network_config(NetworkConfiguration::chaos())
        .with_workload(|...| async move {
            // Send messages
            // Inject disconnection via buggify
            // Verify reconnection and message delivery
        })
        .run();
}

// Test: Endpoint not found handling
#[test]
fn test_endpoint_not_found() {
    // Send to non-existent endpoint
    // Verify EndpointNotFoundReceiver is invoked
    // Verify error propagates to caller
}

// Test: ReplyPromise across network
#[test]
fn test_reply_promise_network() {
    // Client sends request with embedded ReplyPromise
    // Server deserializes, gets ReplyPromise
    // Server fulfills promise
    // Verify networkSender sends response
    // Verify client receives response
}

// Test: Broken promise detection
#[test]
fn test_broken_promise() {
    // Client sends request
    // Server deserializes but drops ReplyPromise without fulfilling
    // Verify broken_promise error sent to client
}
```

## 3.5 Chaos Testing Patterns (From FDB)

```rust
// Connection clogging (FDB ClogSingleConnection pattern)
fn clog_random_pair(sim: &SimWorld) {
    let processes = sim.all_processes();
    let m1 = random_choice(&processes);
    let m2 = random_choice(&processes);
    if m1 != m2 {
        sim.clog_pair(m1, m2, Duration::from_secs(10));
    }
}

// Disconnection injection
fn disconnect_pair_for(sim: &SimWorld, from: &NetworkAddress, to: &NetworkAddress, duration: Duration) {
    sim.disconnect_pair(from, to);
    sim.schedule_after(duration, || sim.reconnect_pair(from, to));
}

// Connection failure window (prevent test thrashing)
fn enable_connection_failures(sim: &SimWorld, duration: Duration) {
    sim.set_connection_failures_enabled(true);
    sim.schedule_after(duration, || sim.set_connection_failures_enabled(false));
}
```

---

# PHASE 4: Migration Steps

## Step 1: Foundation Types & Wire Format ✅ DONE
1. ✅ Create `types.rs` with UID, Endpoint, NetworkAddress
2. ✅ Create `well_known.rs` with WellKnownToken enum
3. ✅ Create `wire.rs` with FDB-compatible packet format
4. ✅ Add crc32c dependency to Cargo.toml
5. ✅ Write unit tests for wire format

> Note: Organized into `messaging/` module. Simplified Endpoint to hold single NetworkAddress instead of NetworkAddressList.

## Step 2: Refactor Peer ✅ DONE
1. ✅ Modify `peer/core.rs` to use new wire format
2. ✅ Change API to `send_reliable(token, bytes)`, `send_unreliable(token, bytes)`
3. ✅ Update receive to return `(UID, Vec<u8>)`
4. ⏳ Update peer tests (will do in Step 4.5)

> Note: Added read buffer for packet framing with `try_deserialize_packet`. Wire format checksum validation on receive.

## Step 3: Remove Old Transport Layer ✅ DONE
1. ✅ Delete `network/transport/` directory entirely
2. ✅ Update `network/mod.rs` exports
3. ✅ Update `lib.rs` exports
4. ✅ Fix any compilation errors

> Note: Also deleted old simulation tests (ping-pong, transport_e2e) that depended on transport layer.

## Step 4: Update Foundation Tests ✅ DONE (partial)
1. ⏳ Update existing tests to use new API (will do when refactoring Peer)
2. ✅ Remove tests for deleted code
3. ⏳ Add new tests for raw transport (will do when refactoring Peer)

## Step 4.1: FDB Chaos Injection Parity ✅

Add missing chaos injection features from FDB's simulation to moonpool-foundation.

### 4.1.1 Network Bit Flipping ✅
**Why**: Tests that checksum validation correctly catches corruption. Without this, checksum code paths are never exercised under chaos.
- Add `BUGGIFY_WITH_PROB(0.0001)` bit flipping at packet receive
- Power-law distribution for number of bits (1-32, biased toward fewer)
- Skip for stable connections
- Verify checksum catches corruption and throws error
- **FDB ref**: `FlowTransport.actor.cpp:1288-1332`
- **Implemented**: Bit flipping in `sim.rs:DataDelivery`, FDB connection teardown pattern in `peer/core.rs`

### 4.1.2 Partial/Short Writes ✅
**Why**: Tests that application code correctly handles `write()` returning less than requested (TCP backpressure, slow receiver).
- BUGGIFY truncates writes to random smaller amounts (0-1000 bytes)
- Caller must check return value and retry with remaining bytes
- **FDB ref**: `sim2.actor.cpp:427-441`
- **Implemented**: Simulation layer truncation in `sim.rs:ProcessSendBuffer`, configurable via `NetworkConfiguration.partial_write_max_bytes`

### 4.1.3 Random Connection Failures ✅
**Why**: Real connections fail randomly during I/O, not just via explicit partitions. Tests recovery code paths.
- 0.001% probability per read/write operation
- Asymmetric failures: send-only, recv-only, or both directions
- Respects cooldown period after failures
- **FDB ref**: `sim2.actor.cpp:580-605`
- **Implemented**: `roll_random_close()` in `sim.rs`, `close_connection_asymmetric()` API, directional closure via `send_closed`/`recv_closed` fields, configurable via `NetworkConfiguration.random_close_probability/cooldown/explicit_ratio`

### 4.1.4 Clock Drift ✅
**Why**: Real clocks drift. Tests time-sensitive code (timeouts, leases, distributed consensus).
- `timer()` drifts 0-0.1s ahead of `now()`
- Simulates clock skew between processes
- **FDB ref**: `sim2.actor.cpp:1058-1064`
- **Implemented**: `timer()` and `now()` on `SimWorld`/`TimeProvider`, FDB formula `timerTime += random01() * (time + 0.1 - timerTime) / 2.0`, `ChaosConfiguration.clock_drift_enabled/max`, comprehensive tests in `tests/clock_drift.rs`

### 4.1.5 Buggified Delays ✅
**Why**: Tests timeout handling and retry logic under unexpected latency spikes.
- 25% chance of extra delay on sleep/timer operations
- Uses power distribution for delay amount: `MAX_DELAY * pow(random01(), 1000.0)`
- **FDB ref**: `sim2.actor.cpp:1100-1105`
- **Implemented**: `apply_buggified_delay()` in `sim.rs:sleep()`, configurable via `ChaosConfiguration.buggified_delay_enabled/max/probability`, comprehensive tests in `tests/chaos_buggified_delay.rs`

### 4.1.6 Connection Establishment Failures ✅
**Why**: `connect()` can fail or hang forever in real networks. Tests connection retry logic.
- Configurable failure modes: throw error (mode 1), hang forever or error (mode 2), disabled (mode 0)
- Mode 2: 50% throw `ConnectionRefused`, 50% hang forever (`std::future::pending`)
- **FDB ref**: `sim2.actor.cpp:1243-1250` (SIM_CONNECT_ERROR_MODE)
- **Implemented**: Failure injection in `network/sim/provider.rs:connect()`, configurable via `ChaosConfiguration.connect_failure_mode/probability`, comprehensive tests in `tests/chaos_connect_failure.rs`

### 4.1.7 Document All Fault Injection ✅
- Created `docs/fault-injection.md` describing all 11 chaos mechanisms
- Created `fault_injection` module in lib.rs for cargo doc integration
- Added summary table to crate-level docs
- Includes: trigger conditions, probabilities, what each tests, FDB references
- Covers: TCP latencies, random close, bit flip, connect failure, clock drift, buggified delays, partial writes, clogging, partitions, peer write failures, message delivery scheduling

## Step 4.2: Foundation Crate Reorganization ✅

Reorganized moonpool-foundation to improve maintainability by splitting large files and grouping related modules.

### Final Directory Structure

```
moonpool-foundation/src/
├── lib.rs                    # Public API exports only
├── error.rs                  # SimulationError, SimulationResult
│
├── sim/                      # Core simulation engine (was sim.rs ~2200 lines)
│   ├── mod.rs               # exports
│   ├── world.rs             # SimWorld, WeakSimWorld, SimInner
│   ├── wakers.rs            # WakerRegistry and waker management
│   ├── events.rs            # Event, EventQueue, ScheduledEvent, NetworkOperation
│   ├── state.rs             # NetworkState, ConnectionState, ClogState, PartitionState
│   ├── sleep.rs             # SleepFuture
│   └── rng.rs               # Thread-local RNG functions
│
├── runner/                   # Simulation runners (was runner.rs ~1176 lines)
│   ├── mod.rs               # exports
│   ├── builder.rs           # SimulationBuilder, IterationControl
│   ├── orchestrator.rs      # DeadlockDetector, WorkloadOrchestrator, IterationManager, MetricsCollector
│   ├── topology.rs          # WorkloadTopology, Workload, TopologyFactory
│   ├── report.rs            # SimulationMetrics, SimulationReport
│   └── tokio.rs             # TokioRunner, TokioReport
│
├── chaos/                    # Testing utilities & fault injection (was scattered)
│   ├── mod.rs               # exports + macros
│   ├── assertions.rs        # always_assert!, sometimes_assert!
│   ├── buggify.rs           # buggify!, buggify_with_prob!
│   ├── invariants.rs        # InvariantCheck type
│   └── state_registry.rs    # StateRegistry for invariants
│
├── messaging/                # FDB-compatible wire format (unchanged)
│   ├── mod.rs
│   ├── types.rs
│   ├── well_known.rs
│   └── wire.rs
│
├── network/                  # Network abstractions (unchanged)
│   ├── mod.rs
│   ├── config.rs
│   ├── traits.rs
│   ├── peer/
│   ├── sim/
│   └── tokio/
│
├── time/mod.rs              # TimeProvider, SimTimeProvider, TokioTimeProvider (flattened)
├── task/mod.rs              # TaskProvider, TokioTaskProvider (flattened)
└── random/mod.rs            # RandomProvider, SimRandomProvider (flattened)
```

### Tests Reorganization (mirroring src/)

```
moonpool-foundation/tests/
├── sim.rs + sim/            # Simulation tests
│   ├── determinism.rs       # Deterministic execution tests
│   ├── integration.rs       # SimWorld integration tests
│   ├── rng.rs               # Thread-local RNG tests
│   └── sleep.rs             # Sleep functionality tests
│
├── chaos.rs + chaos/        # Chaos testing
│   ├── assertions.rs        # Assertion integration tests
│   ├── buggify.rs           # Buggify integration tests
│   ├── bit_flip.rs          # Bit flip chaos tests
│   ├── random_close.rs      # Random close chaos tests
│   ├── connect_failure.rs   # Connect failure chaos tests
│   ├── buggified_delay.rs   # Buggified delay chaos tests
│   └── clock_drift.rs       # Clock drift chaos tests
│
└── network.rs + network/    # Network tests
    ├── traits.rs            # Network provider trait tests
    ├── partition.rs         # Partition integration tests
    └── latency.rs           # Configurable latency tests
```

### Validation Results
- ✅ `cargo fmt` - Clean
- ✅ `cargo clippy` - No warnings
- ✅ `cargo nextest run` - 188 tests passed (89 unit + 99 integration)

## Step 4.5: Build New E2E Simulation Tests ✅ DONE (partial)
> **IMPORTANT**: Use the new Claude skills in `.claude/skills/` for simulation testing:
> - `designing-simulation-workloads` - Design operation alphabets and autonomous workloads
> - `using-buggify` - Add strategic fault injection for chaos testing
> - `using-chaos-assertions` - Track coverage with sometimes_assert! and validate with always_assert!
> - `validating-with-invariants` - Add cross-workload invariant checks
>
> See `.claude/skills/README.md` for learning path and decision flowchart.

### Completed ✅

1. ✅ Created E2E test infrastructure in `tests/e2e/`
   - `mod.rs` - TestMessage format with custom serialization
   - `operations.rs` - Operation alphabet (SendReliable, SendUnreliable, Receive, ForceReconnect, etc.)
   - `invariants.rs` - MessageInvariants for tracking sent/received messages
   - `workloads.rs` - Client and server workloads
   - `tests.rs` - Simulation tests with chaos

2. ✅ Created wire-protocol-aware server (`wire_server_workload`)
   - Uses `try_deserialize_packet()` to parse FDB wire format
   - Validates checksums via wire format
   - Parses TestMessage from payload
   - Tracks received messages for invariant checking

3. ✅ Added sometimes_assert! coverage
   - `checksum_caught_corruption` - Wire checksum validation
   - `connection_teardown_on_wire_error` - FDB pattern for corrupt connections
   - `unreliable_discarded_on_error` - Unreliable message drop on failure
   - `wire_server_connection_closed` - Server sees connection closes
   - `wire_server_wire_error` - Server sees wire format errors
   - `receiver_sees_duplicates` - Duplicates from retransmission
   - `receiver_no_duplicates` - Clean delivery (no chaos)

4. ✅ Chaos testing with `UntilAllSometimesReached(1000)`
   - Tests: reliable_delivery, unreliable_drops, mixed_queues, reconnection, multi_client

### Pending: Cross-Workload Validation

**Current limitation**: Client and server track invariants locally but don't compare across workloads.

The server validates:
- Wire format integrity (checksum via `try_deserialize_packet`)
- TestMessage parsing
- Duplicate tracking (as `sometimes_assert`, not `always_assert`)

But we do NOT validate:
- `client.reliable_sent ⊆ server.reliable_received` at quiescence
- Ordering preservation across workloads
- Global message accounting

**Future enhancement**: Use `StateRegistry` for cross-workload validation:
1. Client publishes `{ reliable_sent: [...], unreliable_sent: [...] }` to StateRegistry
2. Server publishes `{ reliable_received: [...], unreliable_received: [...] }` to StateRegistry
3. Add invariant check that compares client.sent vs server.received after simulation

This requires StateRegistry integration in workloads, which can be implemented independently as a follow-up task.

## Step 5: Moonpool Messaging Module Structure ✅ DONE
1. ✅ Create `moonpool/src/messaging/mod.rs`
2. ✅ Create `moonpool/src/messaging/static/mod.rs`
3. ✅ Create `moonpool/src/messaging/virtual/mod.rs` (placeholder - deferred to future phase)
4. ✅ Update `moonpool/Cargo.toml` to depend on moonpool-foundation

## Step 6: Moonpool Static Messaging Implementation (Phase A: Core) ✅ DONE

### 6.1 EndpointMap ✅ DONE
- ✅ O(1) array lookup for well-known tokens (0-63)
- ✅ HashMap for dynamic endpoints
- ✅ `MessageReceiver` trait for byte-level dispatch
- ✅ 11 unit tests

### 6.2 FlowTransport ✅ DONE
- ✅ Central coordinator managing peers and endpoints
- ✅ `TransportData` struct for internal state (FDB pattern)
- ✅ Synchronous send API (FDB: never await on send)
- ✅ Local delivery optimization (same address = direct dispatch)
- ✅ Lazy peer creation via `get_or_open_peer()` (FDB connectionKeeper pattern)
- ✅ 7 unit tests

### 6.3 NetNotifiedQueue ✅ DONE
- ✅ Type-safe message queue with async notification
- ✅ Waker-based async `recv()` future
- ✅ JSON deserialization on receive
- ✅ Queue closing with proper waker notification
- ✅ `SharedNetNotifiedQueue` wrapper for registration
- ✅ 11 unit tests

### Phase B: Request-Response (Deferred)
The following are deferred per user's "minimal first" preference:
- `request_stream.rs` - RequestStream<T>
- `reply_promise.rs` - ReplyPromise<T> + networkSender
- `interface.rs` - Interface macro

## Step 7: Static Messaging Tests
### 7a: Core Infrastructure Tests ✅ DONE

1. ✅ Simulation tests for EndpointMap + FlowTransport + NetNotifiedQueue
2. ✅ Chaos testing with connection failures
3. ✅ Local delivery under chaos

**Test Infrastructure** (`moonpool/tests/simulation/`):
- `mod.rs` - TestMessage with JSON serialization
- `invariants.rs` - MessageInvariants tracking with always_assert!/sometimes_assert!
- `operations.rs` - ClientOp/ServerOp alphabet with weighted selection
- `workloads.rs` - local_delivery_workload, endpoint_lifecycle_workload
- `test_scenarios.rs` - 6 test scenarios (4 basic + 2 chaos)

**Component Assertions** (18 total):
- EndpointMap (4): well_known_registered, dynamic_lookup_found, well_known_removal_rejected, endpoint_deregistered
- FlowTransport (8): local_delivery_path, remote_peer_path, endpoint_found_local, endpoint_not_found_local, peer_created, peer_reused, dispatch_success, dispatch_undelivered
- NetNotifiedQueue (6): deserialization_success, deserialization_failed, waker_notified, message_available, queue_closed_empty, recv_pending

**Validation**: 47 tests pass (29 unit + 18 simulation), fmt clean, clippy clean

### 7b: Request-Response RPC (Phase 12B) ✅ DONE

**Phase 12B Implementation Complete**:
1. ✅ moonpool-traits crate with MessageCodec trait
2. ✅ NetNotifiedQueue refactored for pluggable codec
3. ✅ Foundation types serde support (UID, NetworkAddress, Endpoint)
4. ✅ ReplyError types (BrokenPromise, ConnectionFailed, Timeout, Serialization, EndpointNotFound)
5. ✅ ReplyPromise with Drop-based broken promise detection
6. ✅ ReplyFuture for client-side response awaiting
7. ✅ RequestStream with RequestEnvelope unpacking
8. ✅ send_request() function
9. ✅ Integration tests (5 tests passing)

**New Files**:
- `moonpool-traits/` - Pluggable serialization trait crate
- `moonpool/src/messaging/static/reply_error.rs`
- `moonpool/src/messaging/static/reply_promise.rs`
- `moonpool/src/messaging/static/reply_future.rs`
- `moonpool/src/messaging/static/request_stream.rs`
- `moonpool/src/messaging/static/request.rs`
- `moonpool/tests/rpc_integration.rs`

**Validation**: All tests pass, fmt clean, clippy clean

### 7c: RPC Simulation Tests (In Progress)

**Goal**: Add simulation tests for RPC layer under chaos conditions.

**Approach**: Extend existing `simulation/` infrastructure:
- Add RPC message types (RpcTestRequest, RpcTestResponse)
- Add RPC operations (SendRequest, AwaitResponse, DropPromise)
- Add RPC invariants (correlation, broken promise tracking)
- Add RPC workload exercising full request-response flow
- Add 5 test scenarios (2 basic + 3 chaos)

**sometimes_assert! coverage** (10 total):
- Existing in RPC code (4): reply_sent, broken_promise_detected, reply_received, reply_queue_closed
- New in simulation (6): rpc_request_sent, rpc_response_received, rpc_request_timeout, rpc_server_received, rpc_broken_promise_path, rpc_success_path

**FDB Invariants to validate**:
1. Request-Response Correlation - every response maps to sent request
2. Promise Single-Fulfillment - at most one of send/error/drop per promise
3. Broken Promise Guarantee - dropped promises send BrokenPromise error
4. No Phantom Messages - can't receive response for unsent request

## Step 8: Migrate Existing Tests (Deferred)
1. Update ping-pong tests to use new RPC layer
2. Verify all simulation tests pass
3. Verify determinism is preserved

---

# Files Summary

## Foundation - DELETE
```
moonpool-foundation/src/network/transport/  (entire directory)
```

## Foundation - NEW
```
moonpool-foundation/src/types.rs
moonpool-foundation/src/well_known.rs
moonpool-foundation/src/wire.rs
```

## Foundation - MODIFY
```
moonpool-foundation/src/lib.rs
moonpool-foundation/src/network/mod.rs
moonpool-foundation/src/network/peer/core.rs
moonpool-foundation/Cargo.toml  (add crc32c)
```

## Moonpool - NEW
```
moonpool/src/lib.rs
moonpool/src/error.rs
moonpool/src/messaging/mod.rs
moonpool/src/messaging/static/mod.rs
moonpool/src/messaging/static/endpoint_map.rs
moonpool/src/messaging/static/flow_transport.rs
moonpool/src/messaging/static/net_notified_queue.rs
moonpool/src/messaging/static/request_stream.rs    # Phase 12B
moonpool/src/messaging/static/reply_promise.rs     # Phase 12B
moonpool/src/messaging/static/reply_future.rs      # Phase 12B
moonpool/src/messaging/static/reply_error.rs       # Phase 12B
moonpool/src/messaging/static/request.rs           # Phase 12B
moonpool/src/messaging/virtual/mod.rs              # Placeholder for future
moonpool/Cargo.toml
moonpool/tests/rpc_integration.rs                  # Phase 12B
```

## moonpool-traits - NEW (Phase 12B)
```
moonpool-traits/Cargo.toml
moonpool-traits/src/lib.rs
moonpool-traits/src/codec.rs
```

---

# FDB Reference Files

When implementing, reference these FDB files:

| Moonpool Component | FDB Reference |
|-------------------|---------------|
| UID | `flow/flow.h` (UID struct) |
| Endpoint | `fdbrpc/FlowTransport.h:Endpoint` |
| Wire format | `fdbrpc/FlowTransport.actor.cpp:scanPackets` |
| EndpointMap | `fdbrpc/FlowTransport.actor.cpp:92-232` |
| Well-known tokens | `fdbrpc/WellKnownEndpoints.h` |
| RequestStream | `fdbrpc/fdbrpc.h:RequestStream` |
| ReplyPromise | `fdbrpc/fdbrpc.h:ReplyPromise` |
| networkSender | `fdbrpc/genericactors.actor.h:networkSender` |
| Peer reconnection | `fdbrpc/FlowTransport.actor.cpp:connectionKeeper` |
| Ping workload | `fdbserver/workloads/Ping.actor.cpp` |
| Clog testing | `fdbserver/workloads/ClogSingleConnection.actor.cpp` |
