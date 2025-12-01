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

### 7c: RPC Simulation Tests ✅ DONE

**Goal**: Add simulation tests for RPC layer under chaos conditions.

**Implementation** (`moonpool/tests/simulation/`):
- `mod.rs` - Added RpcTestRequest, RpcTestResponse message types
- `operations.rs` - Added RpcClientOp, RpcServerOp, weights, generators
- `invariants.rs` - Added RpcInvariants with FDB-style validation
- `workloads.rs` - Added rpc_workload with happy_path and broken_promise_focused configs
- `test_scenarios.rs` - Added 4 test scenarios (2 basic + 2 chaos)

**sometimes_assert! coverage** (10 total):
- Existing in RPC code (4): reply_sent, broken_promise_detected, reply_received, reply_queue_closed
- New in simulation (6): rpc_request_sent, rpc_server_received_request, rpc_response_sent, rpc_promise_dropped, rpc_broken_promise_path, rpc_success_path

**FDB Invariants validated**:
1. ✅ Request-Response Correlation - every response maps to sent request (no_phantom_rpc_response)
2. ✅ Promise Single-Fulfillment - at most one of send/error/drop per promise (rpc_single_resolution)
3. ✅ Broken Promise Guarantee - dropped promises tracked (no_phantom_broken_promise)
4. ✅ No Phantom Messages - can't receive response for unsent request

**Test Scenarios**:
- `test_rpc_basic_request_response` - Happy path validation
- `test_rpc_broken_promise` - Server drops promise testing
- `slow_simulation_rpc_happy_path` - Chaos test for success paths
- `slow_simulation_rpc_error_paths` - Chaos test for error paths

**Limitation**: Tests are local-only (single FlowTransport). Multi-node RPC testing deferred to Step 7d.

**Validation**: 92 tests pass, fmt clean, clippy clean

### 7d: Multi-Node RPC Simulation Tests ✅ DONE

**Goal**: Test RPC across separate nodes with actual network transport.

**Status**: Complete - transport layer fixes implemented, tests passing.

**Implementation** (FDB patterns):

1. **`connection_incoming()` now uses `Peer::new_incoming(stream)`**
   - Location: `moonpool/src/messaging/static/flow_transport.rs:655-720`
   - Uses accepted TCP stream instead of creating outbound connection
   - Follows FDB FlowTransport.actor.cpp:1123 `Peer::onIncomingConnection`

2. **`SimTcpListener` returns synthesized ephemeral addresses**
   - FDB Pattern (sim2.actor.cpp:1149-1175): Server-side connections see synthesized ephemeral addresses
   - IP: base client IP + random offset (0-255)
   - Port: random in range 40000-60000
   - As FDB notes: "In the case of an incoming connection, this may not be an address we can connect to!"

3. **`ConnectionState` now tracks `peer_address`**
   - For client-side: server's listening address
   - For server-side: synthesized ephemeral address
   - Stored during `create_connection_pair()`

**Files Modified**:
- `moonpool/src/messaging/static/flow_transport.rs` - Use `Peer::new_incoming(stream)`
- `moonpool-foundation/src/sim/state.rs` - Add `peer_address` field
- `moonpool-foundation/src/sim/world.rs` - Synthesize ephemeral addresses, add getter
- `moonpool-foundation/src/network/sim/stream.rs` - Return stored peer address
- `moonpool/tests/simulation/workloads.rs` - Fix server invariant validation
- `.config/nextest.toml` - Add timeout overrides for multi-node tests

**Tests Enabled**:
- `test_multi_node_rpc_1x1` - Basic 1 client + 1 server ✅
- `test_multi_node_rpc_2x1` - Load test 2 clients + 1 server ✅
- `slow_simulation_multi_node_rpc` - Full chaos testing ✅

**sometimes_assert! coverage** (multi-node assertions):
- multi_node_server_listening
- multi_node_server_received_request
- multi_node_server_sent_response
- multi_node_server_dropped_promise
- multi_node_client_sent_request
- multi_node_has_clients
- multi_node_has_servers
- multi_node_requests_sent
- multi_node_responses_sent

**Validation**: 317 tests pass, fmt clean, clippy clean

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

---

# Phase 12C: Developer Experience Improvements ✅ DONE

Identified pain points from the `ping_pong.rs` real TCP example.

## Step 9: FlowTransportBuilder (Pain Points 1 & 4) ✅ DONE

**Problem**: Manual `Rc` wrapping and `set_weak_self()` is error-prone. Forgetting `set_weak_self()` causes runtime panics. Clients must also call `listen()` to receive responses, which is not obvious.

**Solution**: Created `FlowTransportBuilder` that:
- [x] Automatically wraps in `Rc`
- [x] Automatically calls `set_weak_self()`
- [x] Provides `build()` for fire-and-forget senders (no listening)
- [x] Provides `build_listening()` for RPC (servers AND clients that need responses)
- [ ] Optional `with_tokio()` helper (deferred - requires more design work)

**Files**:
- `moonpool/src/messaging/static/flow_transport.rs:600-701` - FlowTransportBuilder

**API**:
```rust
// For RPC (servers and clients):
let transport = FlowTransportBuilder::new(network, time, task)
    .local_address(addr)
    .build_listening()
    .await?;

// For fire-and-forget:
let transport = FlowTransportBuilder::new(network, time, task)
    .local_address(addr)
    .build();
```

## Step 10: Static Interface Registration (Pain Point 2) ✅ DONE

**Problem**: Multiple steps to create an endpoint, and UIDs are manually created without type safety.

### 10a: Simple API with user-provided token ✅ DONE
```rust
// Server registers handler - single call replaces 5 lines of boilerplate
let ping = transport.register_handler::<PingRequest>(ping_token(), JsonCodec);
```

**Files**:
- `moonpool/src/messaging/static/flow_transport.rs:265-306` - register_handler

### 10b: Multi-method interfaces with method index ✅ DONE
```rust
const CALC_INTERFACE: u64 = 0xCA1C_0000;
const METHOD_ADD: u64 = 0;
const METHOD_SUB: u64 = 1;

// Each method gets deterministic UID from interface_id + method_index
let (add_stream, add_token) = transport.register_handler_at::<AddRequest>(
    CALC_INTERFACE, METHOD_ADD, JsonCodec
);
let (sub_stream, sub_token) = transport.register_handler_at::<SubRequest>(
    CALC_INTERFACE, METHOD_SUB, JsonCodec
);
```

**Files**:
- `moonpool/src/messaging/static/flow_transport.rs:308-370` - register_handler_at

### 10c: Interface macro (future syntactic sugar)
Deferred - validate the pattern first before adding syntactic sugar.

## Step 11: Embedded Transport in RequestStream (Pain Point 3) ✅ DONE

**Problem**: `RequestStream::recv()` requires a closure callback to send responses:
```rust
// BEFORE (awkward):
let transport_clone = transport.clone();
ping_stream.recv(move |endpoint, payload| {
    transport_clone.send_reliable(endpoint, payload);
})
```

**Solution**: Added `recv_with_transport()` methods that hide the closure:
```rust
// AFTER (clean):
let (req, reply) = ping_stream.recv_with_transport(&transport).await?;
reply.send(response);
```

**Files**:
- `moonpool/src/messaging/static/request_stream.rs:119-170` - recv_with_transport, try_recv_with_transport

**Note**: ReplyPromise still uses closure internally but the API hides it. True embedded transport would require making ReplyPromise generic over transport types, adding complexity.

## Step 12: Calculator Example (Multi-Handler Validation) ✅ DONE

**Goal**: Create `examples/calculator.rs` to validate multi-handler pattern with `select!`.

**Completed**:
- [x] Create calculator example with Add, Sub, Mul, Div operations
- [x] Demonstrate `tokio::select!` on multiple handlers
- [x] Show client calling different endpoints
- [x] Validate the static interface ID pattern works in practice

**Files**:
- `moonpool/examples/calculator.rs` - Full multi-method RPC example
- `moonpool/examples/ping_pong.rs` - Updated to use new APIs

## Final API (Implemented)

### Single Handler (Ping)
```rust
// === SERVER ===
let transport = FlowTransportBuilder::new(network, time, task)
    .local_address(local_addr)
    .build_listening()
    .await?;

// Single call registers handler
let ping_stream = transport.register_handler::<PingRequest>(ping_token(), JsonCodec);

loop {
    // No closure needed
    let (request, reply) = ping_stream
        .recv_with_transport::<_, _, _, PingResponse>(&transport)
        .await?;
    reply.send(PingResponse { echo: request.message });
}

// === CLIENT ===
let transport = FlowTransportBuilder::new(network, time, task)
    .local_address(local_addr)
    .build_listening()
    .await?;

let server_endpoint = Endpoint::new(server_addr, ping_token());
let response = send_request(&transport, &server_endpoint, request, JsonCodec)?
    .await?;
```

### Multi-Handler (Calculator)
```rust
// === SERVER ===
let transport = FlowTransportBuilder::new(network, time, task)
    .local_address(local_addr)
    .build_listening()
    .await?;

// Register multiple handlers with deterministic tokens
let (add_stream, _) = transport.register_handler_at::<AddRequest>(CALC, METHOD_ADD, JsonCodec);
let (sub_stream, _) = transport.register_handler_at::<SubRequest>(CALC, METHOD_SUB, JsonCodec);
let (mul_stream, _) = transport.register_handler_at::<MulRequest>(CALC, METHOD_MUL, JsonCodec);
let (div_stream, _) = transport.register_handler_at::<DivRequest>(CALC, METHOD_DIV, JsonCodec);

loop {
    tokio::select! {
        Some((req, reply)) = add_stream.recv_with_transport(&transport) => {
            reply.send(AddResponse { result: req.a + req.b });
        }
        Some((req, reply)) = sub_stream.recv_with_transport(&transport) => {
            reply.send(SubResponse { result: req.a - req.b });
        }
        // ... more handlers
    }
}

// === CLIENT ===
let add_endpoint = Endpoint::new(server_addr, UID::new(CALC, METHOD_ADD));
let response = send_request(&transport, &add_endpoint, AddRequest { a: 10, b: 5 }, JsonCodec)?
    .await?;
```

## Validation

- ✅ All 324 tests pass
- ✅ `cargo fmt` clean
- ✅ `cargo clippy` clean (existing warnings only)
- ✅ `ping_pong.rs` updated to use new APIs
- ✅ `calculator.rs` validates multi-handler pattern

---

# Phase 12D: Testing Improvements

Analysis revealed that while Phase 12C APIs work in examples, simulation tests still use the old manual patterns. This phase migrates tests and adds coverage for chaos scenarios.

## Step 13: Migrate Simulation Tests to Phase 12C APIs ✅ DONE

**Goal**: Validate new APIs work under chaos conditions.

**Sub-tasks**:
- [x] Refactor `multi_node_rpc_server_workload` to use `FlowTransportBuilder::build_listening()`
- [x] Refactor `multi_node_rpc_client_workload` to use `FlowTransportBuilder::build()` (clients don't need listen)
- [x] Replace manual endpoint registration with `register_handler()`
- [x] Replace closure pattern with `recv_with_transport()`

**Files**:
- `moonpool/tests/simulation/workloads.rs`

## Step 14: Add Unit Tests for Phase 12C APIs ✅ DONE

**Goal**: Direct unit test coverage for new APIs.

**Sub-tasks**:
- [x] `test_recv_with_transport` - async receive with transport reference
- [x] `test_try_recv_with_transport` - sync non-blocking version
- [x] `test_recv_with_transport_reply_works` - verify reply.send() actually sends

**Files**:
- `moonpool/src/messaging/static/request_stream.rs` (add to mod tests)

## Step 15: Add Connection Clogging Test ✅ DONE

**Goal**: Test message delivery during network congestion (FDB pattern).

**FDB Reference**: `ClogSingleConnection.actor.cpp`

**Sub-tasks**:
- [x] Clogging covered by existing chaos tests (`slow_simulation_*` with `NetworkConfiguration::chaos()`)
- [x] `SimWorld::clog_pair()` exercised via random clogging in chaos mode
- [x] Message queuing and eventual delivery validated by invariants

**Note**: Dedicated `slow_simulation_rpc_with_clogging` test not needed - clogging is already part of the chaos configuration and exercised in multi-node RPC tests.

**Files**:
- `moonpool/tests/simulation/test_scenarios.rs`

## Step 16: Add Timeout Path Coverage ✅ DONE

**Goal**: Ensure timeout handling is tested under chaos.

**Sub-tasks**:
- [x] Add `AwaitWithTimeout { request_id, timeout_ms }` to `RpcClientOp`
- [x] Add `rpc_timeout_path` sometimes_assert trigger
- [x] Test client behavior when server is slow/unresponsive

**Files**:
- `moonpool/tests/simulation/operations.rs`
- `moonpool/tests/simulation/workloads.rs`

## Step 17: Add Multi-Method Interface Simulation Test ✅ DONE

**Goal**: Validate `register_handler_at` pattern under chaos.

**Sub-tasks**:
- [x] Create calculator-style workload (Add, Sub, Mul request types)
- [x] Verify messages route to correct handlers
- [x] Test `tokio::select!` on multiple streams under chaos

**Files**:
- `moonpool/tests/simulation/mod.rs` (calculator types)
- `moonpool/tests/simulation/test_scenarios.rs` (multi_method_workload, tests)

## Step 18: Add Builder Error Path Tests ✅ DONE

**Goal**: Test error handling in FlowTransportBuilder.

**Sub-tasks**:
- [x] Test `build_listening()` when port already in use (bind error)
- [x] Test `build()` without `local_address()` panics with clear message
- [x] Test `build_listening()` with SimNetworkProvider

**Files**:
- `moonpool/src/messaging/static/flow_transport.rs` (mod tests)
- `moonpool/tests/simulation/test_scenarios.rs` (SimNetworkProvider test)

---

# Phase 12E: TCP Simulation Layer Improvements

## Overview

Extend the TCP simulation layer with additional failure modes inspired by FoundationDB's sim2. These improvements increase bug-finding capability by simulating more realistic network failure scenarios.

### References
- `docs/references/foundationdb/FlowTransport.actor.cpp` - Connection handling patterns
- `docs/references/foundationdb/Net2.actor.cpp` - Simulation implementation
- `docs/references/foundationdb/sim2.actor.cpp` - Fault injection

### Goals
1. Fix existing copy-paste bug in stream.rs
2. Add symmetric read clogging (complement to existing write clogging)
3. Implement TCP-level failure modes for better bug detection
4. Improve logging for easier debugging

### Verification Summary

**Already Exists** (no work needed):
- Write clogging (`is_write_clogged`, `should_clog_write`, `clog_write`, `register_clog_waker`)
- IP-level partitions (`partition_pair`, `partition_send_from`, `partition_recv_to`)
- Asymmetric closure (`close_connection_asymmetric`)
- Partial writes (`partial_write_max_bytes` + buggify truncation)
- Connection establishment failures (in `NetworkConfiguration`)
- Buggified delays

**Missing** (to be implemented):
- `is_connection_cut()` method - core bug fix
- Read clogging (symmetric with write)
- Per-connection asymmetric delays
- Per-connection-pair base latency
- Half-open connection simulation
- Send buffer limits (BDP-based)
- RST vs FIN distinction
- Intermittent packet loss (probability-based)

---

## Step 19: Fix Copy-Paste Bug and Add Connection Cut Support

**Goal**: Fix unreachable dead code in stream.rs where `is_connection_closed()` is called twice when the second check should distinguish temporary cuts from permanent closures.

**Bug Location**: `moonpool-foundation/src/network/sim/stream.rs`
- Lines 184-201: `poll_read` first check
- Lines 232-248: `poll_read` recheck section
- Lines 287-306: `poll_write` check

**Root Cause**: `is_connection_cut()` method doesn't exist. The code has comments saying "connection is cut" but calls `is_connection_closed()` making the second branch unreachable.

**Sub-tasks**:
- [ ] Add `is_cut: bool` field to `ConnectionState` in `state.rs`
- [ ] Add `cut_connection(ConnectionId, Duration)` method to SimWorld for temporary cuts
- [ ] Add `is_connection_cut(ConnectionId)` method to SimWorld
- [ ] Add `restore_connection(ConnectionId)` method to restore cut connections
- [ ] Add `register_cut_waker(ConnectionId, Waker)` for wakeup on restore
- [ ] Fix `stream.rs:poll_read` to use `is_connection_cut()` for second check
- [ ] Fix `stream.rs:poll_write` to use `is_connection_cut()` for second check
- [ ] Add tests for cut vs closed behavior

**Files**:
- `moonpool-foundation/src/sim/state.rs`
- `moonpool-foundation/src/sim/world.rs`
- `moonpool-foundation/src/network/sim/stream.rs`

---

## Step 20: Add Read Clogging (Symmetric with Write)

**Goal**: Implement read clogging to complement existing write clogging. When read is clogged, `poll_read` returns `Poll::Pending` even if data is available in the buffer.

**FDB Reference**: Symmetric with write clogging in `FlowTransport.actor.cpp`

**Sub-tasks**:
- [ ] Add `read_clogged: bool` field to `ConnectionState`
- [ ] Add `read_clog_expiry: Option<Duration>` field
- [ ] Add `is_read_clogged(ConnectionId)` method
- [ ] Add `should_clog_read(ConnectionId)` probability check (buggify)
- [ ] Add `clog_read(ConnectionId)` method
- [ ] Add `register_read_clog_waker(ConnectionId, Waker)` method
- [ ] Modify `poll_read` to check read clog state
- [ ] Add read clog clearing to `clear_expired_clogs()`
- [ ] Add tests for read clogging behavior

**Files**:
- `moonpool-foundation/src/sim/state.rs`
- `moonpool-foundation/src/sim/world.rs`
- `moonpool-foundation/src/network/sim/stream.rs`

---

## Step 21: Add RST vs FIN Distinction

**Goal**: Distinguish between graceful close (FIN → peer gets EOF) and abrupt close (RST → peer gets ECONNRESET).

**FDB Reference**: `FlowTransport.actor.cpp` connection termination handling

**Sub-tasks**:
- [ ] Add `close_reason: CloseReason` enum to `ConnectionState` (None, Graceful, Aborted)
- [ ] Add `close_connection_graceful(ConnectionId)` method (FIN semantics)
- [ ] Add `close_connection_abort(ConnectionId)` method (RST semantics)
- [ ] Modify `poll_read` to return appropriate error based on close reason
- [ ] Modify `poll_write` to return appropriate error based on close reason
- [ ] Update `Drop` impl to use graceful close by default
- [ ] Add chaos injection to randomly choose RST vs FIN
- [ ] Add tests for both close types

**Files**:
- `moonpool-foundation/src/sim/state.rs`
- `moonpool-foundation/src/sim/world.rs`
- `moonpool-foundation/src/network/sim/stream.rs`

---

## Step 22: Add Send Buffer Limits

**Goal**: Implement realistic send buffer behavior where `poll_write` returns `Pending` when buffer is full.

**FDB Reference**: `Net2Packet.h:43-91` queuing implementation

**Sub-tasks**:
- [ ] Add `send_buffer_capacity: usize` to `ConnectionState`
- [ ] Calculate capacity using BDP: `latency × bandwidth` (FDB pattern)
- [ ] Add `send_buffer_capacity(ConnectionId)` method
- [ ] Add `send_buffer_used(ConnectionId)` method
- [ ] Add `available_send_buffer(ConnectionId)` method
- [ ] Modify `poll_write` to return `Pending` when buffer full
- [ ] Add `register_send_buffer_waker(ConnectionId, Waker)` for wakeup when space available
- [ ] Add tests for buffer backpressure behavior

**Files**:
- `moonpool-foundation/src/sim/state.rs`
- `moonpool-foundation/src/sim/world.rs`
- `moonpool-foundation/src/network/sim/stream.rs`

---

## Step 23: Add Per-Connection Asymmetric Delays

**Goal**: Allow send delay ≠ receive delay per connection for more realistic simulation.

**Sub-tasks**:
- [ ] Add `send_delay: Duration` field to `ConnectionState`
- [ ] Add `recv_delay: Duration` field to `ConnectionState`
- [ ] Add `get_send_delay(ConnectionId)` method
- [ ] Add `get_recv_delay(ConnectionId)` method
- [ ] Add `set_asymmetric_delays(ConnectionId, send: Duration, recv: Duration)` method
- [ ] Modify data delivery to apply appropriate delay per direction
- [ ] Add buggify to randomly set asymmetric delays on connection establishment
- [ ] Add tests for asymmetric delay behavior

**Files**:
- `moonpool-foundation/src/sim/state.rs`
- `moonpool-foundation/src/sim/world.rs`

---

## Step 24: Add Per-Connection-Pair Base Latency

**Goal**: Each connection pair has consistent base latency (set once on establishment), with optional jitter.

**Sub-tasks**:
- [ ] Add `pair_latencies: HashMap<(IpAddr, IpAddr), Duration>` to SimWorld
- [ ] Add `get_connection_latency(ConnectionId)` method
- [ ] Add `set_pair_latency_if_not_set(src: IpAddr, dst: IpAddr, latency: Duration)` method
- [ ] Add `get_pair_latency(src: IpAddr, dst: IpAddr)` method
- [ ] Set latency on connection establishment if not already set
- [ ] Add configurable jitter on top of base latency
- [ ] Add tests for consistent latency behavior

**Files**:
- `moonpool-foundation/src/sim/world.rs`
- `moonpool-foundation/src/network/sim/connect.rs`

---

## Step 25: Add Half-Open Connection Simulation

**Goal**: Simulate peer crash where local side thinks it's connected but remote is gone.

**FDB Reference**: Failure detection patterns in `sim2.actor.cpp`

**Sub-tasks**:
- [ ] Add `simulate_peer_crash(ConnectionId)` method
- [ ] Mark connection such that:
  - Local side still thinks connected
  - Writes eventually return `ECONNRESET` or `ETIMEDOUT` (delayed, not immediate)
  - Reads block, then eventually error
- [ ] Add configurable delay before errors manifest
- [ ] Add tests for half-open detection patterns

**Files**:
- `moonpool-foundation/src/sim/world.rs`
- `moonpool-foundation/src/network/sim/stream.rs`

---

## Step 26: Add Intermittent Packet Loss

**Goal**: Probabilistic packet drop where data is accepted by `poll_write` but never delivered.

**Sub-tasks**:
- [ ] Add `packet_loss_probability: f64` to `NetworkConfiguration`
- [ ] Add `should_drop_packet(ConnectionId)` method using deterministic RNG
- [ ] Modify data delivery path to probabilistically drop
- [ ] Data accepted but silently discarded → eventually causes timeout
- [ ] Add tests verifying timeout behavior under packet loss

**Files**:
- `moonpool-foundation/src/network/config.rs`
- `moonpool-foundation/src/sim/world.rs`

---

## Step 27: Improve Logging

**Goal**: Better logging for debugging without noise during normal operation.

**Sub-tasks**:
- [ ] Change routine poll operations from `info!`/`debug!` to `trace!`
- [ ] Keep `info!` only for state changes (connection established, closed, cut, etc.)
- [ ] Add connection ID to all error messages
- [ ] Document logging levels in code comments

**Files**:
- `moonpool-foundation/src/network/sim/stream.rs`
- `moonpool-foundation/src/network/sim/connect.rs`
- `moonpool-foundation/src/network/sim/listener.rs`
