# Phase 11: NetTransport Integration for Single Server Test

## Overview
Build a transport layer using Sans I/O architecture and integrate it with the existing single server ping_pong test. Replace the server's direct `NetworkProvider::bind()` + `listener.accept()` pattern with NetTransport abstraction (FlowTransport equivalent), while keeping all Sans I/O components and architecture.

## Sans I/O Core Design Principles

### Separation of Concerns
- **Pure State Machine**: Protocol logic with zero I/O dependencies
- **I/O Driver**: Bridges state machine to actual network operations
- **Time as Parameter**: Pass `Instant` instead of calling `time.now()`
- **Deterministic Testing**: Protocol testable without any network I/O

### Architecture
```
Transport Layer:
├── TransportProtocol     (Sans I/O state machine)
├── TransportDriver       (I/O integration layer)
├── NetTransport trait    (FlowTransport equivalent API)
└── Existing Peer         (Connection I/O via connection_task)
```

## Phase 0: EnvelopeSerializer Trait Foundation

### Design Requirement
The serialization layer must be swappable to allow future enhancements (Orleans-style rich metadata) or custom user implementations without modifying the core protocol logic.

### Core Serialization Trait (`transport/envelope.rs`)
```rust
use std::fmt::Debug;

/// Trait for swappable envelope serialization strategies
/// Users can implement this trait to provide custom envelope formats
pub trait EnvelopeSerializer: Clone {
    type Envelope: Debug + Clone;
    
    /// Serialize envelope to bytes for network transmission
    fn serialize(&self, envelope: &Self::Envelope) -> Vec<u8>;
    
    /// Deserialize bytes back to envelope
    fn deserialize(&self, data: &[u8]) -> Result<Self::Envelope, SerializationError>;
}

#[derive(Debug)]
pub enum SerializationError {
    TooShort,
    InvalidFormat,
    InvalidUtf8,
}
```

### Request-Response Implementation (`transport/request_response_envelope.rs`)
```rust
/// Request-response envelope for correlation (FDB-inspired)
#[derive(Debug, Clone)]
pub struct RequestResponseEnvelope {
    pub destination_token: u64,    // Where this message goes
    pub source_token: u64,         // Where response should go back  
    pub payload: Vec<u8>,          // Raw application data
}

#[derive(Clone)]
pub struct RequestResponseSerializer;

impl EnvelopeSerializer for RequestResponseSerializer {
    type Envelope = RequestResponseEnvelope;
    
    fn serialize(&self, envelope: &Self::Envelope) -> Vec<u8> {
        let mut buf = Vec::with_capacity(20 + envelope.payload.len());
        
        // Fixed header: [dest:8][source:8][len:4][payload:N]
        buf.extend_from_slice(&envelope.destination_token.to_le_bytes());
        buf.extend_from_slice(&envelope.source_token.to_le_bytes());
        buf.extend_from_slice(&(envelope.payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&envelope.payload);
        
        buf
    }
    
    fn deserialize(&self, data: &[u8]) -> Result<Self::Envelope, SerializationError> {
        if data.len() < 20 {
            return Err(SerializationError::TooShort);
        }
        
        let destination_token = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let source_token = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let payload_len = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
        
        if data.len() < 20 + payload_len {
            return Err(SerializationError::TooShort);
        }
        
        Ok(RequestResponseEnvelope {
            destination_token,
            source_token,
            payload: data[20..20 + payload_len].to_vec(),
        })
    }
}
```

### Future Extensibility

The `EnvelopeSerializer` trait enables:
- **Rich Envelopes**: Future implementation with Orleans-style metadata (correlation IDs, grain addresses, etc.)
- **Custom Formats**: Users can provide their own serialization (protobuf, msgpack, etc.)
- **Protocol Evolution**: Add new envelope types without changing core transport logic

## Phase 1: Sans I/O Protocol State Machine

### Core Types (`transport/types.rs`)
```rust
#[derive(Debug)]
pub struct Transmit {
    pub destination: String,
    pub data: Vec<u8>,             // Serialized envelope
}
```

### Protocol State Machine (`transport/protocol.rs`)
```rust
use std::collections::{HashMap, VecDeque};

pub struct TransportProtocol<S: EnvelopeSerializer> {
    serializer: S,
    next_token: u64,
    
    // Queues work with generic envelopes
    transmit_queue: VecDeque<Transmit>,
    receive_queue: VecDeque<S::Envelope>,
}

impl<S: EnvelopeSerializer> TransportProtocol<S> {
    pub fn new(serializer: S) -> Self {
        Self {
            serializer,
            next_token: 1,
            transmit_queue: VecDeque::new(),
            receive_queue: VecDeque::new(),
        }
    }
    
    // Generate next unique token
    fn next_token(&mut self) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        token
    }
    
    // Send envelope to destination
    pub fn send(&mut self, destination: String, envelope: S::Envelope) {
        let data = self.serializer.serialize(&envelope);
        self.transmit_queue.push_back(Transmit { destination, data });
    }
    
    // Handle received data (pure state transition)
    pub fn handle_received(&mut self, from: String, data: Vec<u8>) {
        match self.serializer.deserialize(&data) {
            Ok(envelope) => self.receive_queue.push_back(envelope),
            Err(e) => tracing::warn!("Failed to deserialize from {}: {:?}", from, e),
        }
    }
    
    // Poll methods for I/O driver
    pub fn poll_transmit(&mut self) -> Option<Transmit> { 
        self.transmit_queue.pop_front() 
    }
    
    pub fn poll_receive(&mut self) -> Option<S::Envelope> { 
        self.receive_queue.pop_front() 
    }
    
    // Handle timeouts (time passed as parameter - Sans I/O)
    pub fn handle_timeout(&mut self, now: Instant) {
        // Future: timeout tracking, cleanup, etc.
    }
}
```

## Phase 2: I/O Driver Integration

### Transport Driver (`transport/driver.rs`)
```rust
pub struct TransportDriver<N, T, TP, S> 
where
    N: NetworkProvider,
    T: TimeProvider, 
    TP: TaskProvider,
    S: EnvelopeSerializer,
{
    // Sans I/O protocol state machine
    protocol: TransportProtocol<S>,
    
    // I/O layer - reuse existing Peer infrastructure
    peers: HashMap<String, Rc<RefCell<Peer<N, T, TP>>>>,
    
    // Providers
    network: N,
    time: T,
    task_provider: TP,
}

impl<N, T, TP, S> TransportDriver<N, T, TP, S>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
    S: EnvelopeSerializer,
{
    pub fn new(serializer: S, network: N, time: T, task_provider: TP) -> Self {
        Self {
            protocol: TransportProtocol::new(serializer),
            peers: HashMap::new(),
            network,
            time,
            task_provider,
        }
    }
    
    // Send envelope to destination (creates peer if needed)
    pub fn send(&mut self, destination: &str, envelope: S::Envelope) {
        // Delegate to Sans I/O protocol (pure)
        self.protocol.send(destination.to_string(), envelope);
        
        // Process queued transmissions (I/O)
        self.process_transmissions();
    }
    
    // Get or create peer for destination
    fn get_or_create_peer(&mut self, destination: &str) -> Rc<RefCell<Peer<N, T, TP>>> {
        if !self.peers.contains_key(destination) {
            let peer = Peer::new(
                self.network.clone(),
                self.time.clone(),
                self.task_provider.clone(),
                destination.to_string(),
                Default::default(),
            );
            self.peers.insert(destination.to_string(), Rc::new(RefCell::new(peer)));
        }
        self.peers[destination].clone()
    }
    
    // Process transmissions from protocol to I/O
    fn process_transmissions(&mut self) {
        while let Some(transmit) = self.protocol.poll_transmit() {
            let peer = self.get_or_create_peer(&transmit.destination);
            if let Ok(mut peer_ref) = peer.try_borrow_mut() {
                let _ = peer_ref.send(transmit.data);
            }
        }
    }
    
    // Callback when peer receives data
    pub fn on_peer_received(&mut self, from: String, data: Vec<u8>) {
        self.protocol.handle_received(from, data);
    }
    
    // Poll for received envelopes
    pub fn poll_receive(&mut self) -> Option<S::Envelope> {
        self.protocol.poll_receive()
    }
    
    // Drive the transport (call periodically)
    pub async fn tick(&mut self) {
        let now = self.time.now();
        self.protocol.handle_timeout(now);
        self.process_transmissions();
    }
}
```

## Phase 3: NetTransport Abstraction

### NetTransport trait (`transport/net_transport.rs`)
Create FlowTransport equivalent API:

```rust
#[async_trait(?Send)]
pub trait NetTransport<S: EnvelopeSerializer> {
    /// Bind to address and start accepting connections (server mode)
    async fn bind(&mut self, address: &str) -> Result<(), TransportError>;
    
    /// Send envelope to destination
    fn send(&mut self, destination: &str, envelope: S::Envelope) -> Result<(), TransportError>;
    
    /// Poll for received envelopes (non-blocking)
    fn poll_receive(&mut self) -> Option<ReceivedEnvelope<S::Envelope>>;
    
    /// Drive transport (call regularly)
    async fn tick(&mut self);
}

#[derive(Debug)]
pub struct ReceivedEnvelope<E> {
    pub from: String,
    pub envelope: E,
}

#[derive(Debug)]
pub enum TransportError {
    BindFailed(String),
    SendFailed(String),
    NotBound,
}
```

### Server NetTransport Implementation (`transport/server.rs`)
```rust
pub struct ServerTransport<N, T, TP, S>
where
    N: NetworkProvider,
    T: TimeProvider, 
    TP: TaskProvider,
    S: EnvelopeSerializer,
{
    driver: TransportDriver<N, T, TP, S>,
    listener: Option<N::TcpListener>,
    bind_address: Option<String>,
}

#[async_trait(?Send)]
impl<N, T, TP, S> NetTransport<S> for ServerTransport<N, T, TP, S>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
    S: EnvelopeSerializer,
{
    async fn bind(&mut self, address: &str) -> Result<(), TransportError> {
        let listener = self.driver.network.bind(address).await
            .map_err(|e| TransportError::BindFailed(format!("{:?}", e)))?;
        self.listener = Some(listener);
        self.bind_address = Some(address.to_string());
        Ok(())
    }
    
    fn send(&mut self, destination: &str, envelope: S::Envelope) -> Result<(), TransportError> {
        self.driver.send(destination, envelope);
        Ok(())
    }
    
    fn poll_receive(&mut self) -> Option<ReceivedEnvelope<S::Envelope>> {
        self.driver.poll_receive().map(|envelope| ReceivedEnvelope {
            from: "client".to_string(), // Simplified for now
            envelope,
        })
    }
    
    async fn tick(&mut self) {
        // Handle incoming connections
        if let Some(ref listener) = self.listener {
            // Non-blocking accept attempt
            // TODO: Implement proper connection acceptance
        }
        
        self.driver.tick().await;
    }
}
```

## Phase 4: Update Single Server Test

### Modify PingPongServerActor (`tests/simulation/ping_pong/actors.rs`)
Replace direct bind/accept with NetTransport:

```rust
// Update PingPongServerActor to use NetTransport
pub struct PingPongServerActor<N, T, TP> 
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
{
    transport: Box<dyn NetTransport<RequestResponseSerializer>>,
    server_token: u64,
    topology: WorkloadTopology,
}

impl<N, T, TP> PingPongServerActor<N, T, TP> 
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
{
    pub fn new(network: N, time: T, task_provider: TP, topology: WorkloadTopology) -> Self {
        let server_transport = ServerTransport::new(
            RequestResponseSerializer,
            network,
            time,
            task_provider,
        );
        
        Self {
            transport: Box::new(server_transport),
            server_token: 1, // Server uses token 1
            topology,
        }
    }

    pub async fn run(&mut self) -> SimulationResult<SimulationMetrics> {
        let server_addr = &self.topology.my_ip;
        
        // Bind to server address using NetTransport
        self.transport.bind(server_addr).await
            .map_err(|e| SimulationError::IoError(format!("Bind failed: {:?}", e)))?;
        
        tracing::debug!("Server: Bound to {} using NetTransport", server_addr);
        
        // Handle incoming messages via transport
        loop {
            self.transport.tick().await;
            
            while let Some(received) = self.transport.poll_receive() {
                let message = String::from_utf8_lossy(&received.envelope.payload);
                
                if message.starts_with("PING-") {
                    self.handle_ping_envelope(&received.from, &received.envelope).await?;
                } else if message == "CLOSE" {
                    self.handle_close_envelope(&received.from, &received.envelope).await?;
                    return Ok(SimulationMetrics::default());
                }
            }
        }
    }
    
    async fn handle_ping_envelope(
        &mut self,
        from: &str,
        envelope: &RequestResponseEnvelope,
    ) -> SimulationResult<()> {
        let message = String::from_utf8_lossy(&envelope.payload);
        if let Some((_, uuid_str)) = message.split_once('-') {
            if let Ok(uuid) = uuid_str.parse::<u128>() {
                tracing::debug!("Server: Processing PING with UUID {}", uuid);
                
                // Send PONG response using envelope
                let response = RequestResponseEnvelope {
                    destination_token: envelope.source_token,
                    source_token: self.server_token,
                    payload: format!("PONG-{}", uuid).into_bytes(),
                };
                
                self.transport.send(from, response)
                    .map_err(|e| SimulationError::IoError(format!("Send failed: {:?}", e)))?;
                
                always_assert!(
                    server_sends_pong,
                    true,
                    "Server should always send PONG responses"
                );
            }
        }
        Ok(())
    }
    
    async fn handle_close_envelope(
        &mut self,
        from: &str,
        _envelope: &RequestResponseEnvelope,
    ) -> SimulationResult<()> {
        tracing::debug!("Server: Received CLOSE message via transport");
        
        always_assert!(
            server_receives_close,
            true,
            "Server should receive CLOSE message"
        );
        
        let bye_response = RequestResponseEnvelope {
            destination_token: 0, // No specific token for BYE
            source_token: self.server_token,
            payload: b"BYE!!".to_vec(),
        };
        
        let _ = self.transport.send(from, bye_response); // May fail if client disconnects
        Ok(())
    }
}

// Client continues to use Peer (unchanged for now)
// This maintains the current client behavior while server uses NetTransport
```

This migration step ensures the existing tests continue working while introducing the envelope concept.

## Phase 5: Client Integration (Future)

In a future phase, the client side can also be updated to use NetTransport instead of direct Peer usage, completing the full transport abstraction.

## Files to Create/Modify

```
moonpool-simulation/src/network/transport/
├── mod.rs                           (re-exports)
├── envelope.rs                      (EnvelopeSerializer trait)
├── request_response_envelope.rs     (RequestResponseEnvelope and RequestResponseSerializer)
├── types.rs                         (Transmit struct)
├── protocol.rs                      (Sans I/O TransportProtocol<S>)
└── driver.rs                         (I/O integration TransportDriver<N,T,TP,S>)

tests/simulation/ping_pong/
└── actors.rs                        (update PingPongServerActor to use NetTransport)
```

## Key Benefits

1. **Pure Testing**: Protocol logic tested without network I/O
2. **Deterministic**: Perfect for simulation (time as parameter)
3. **Separation**: Clean boundary between state and I/O
4. **Reuse**: Builds on existing Peer connection infrastructure
5. **Foundation**: Base for future protocol enhancements

## Implementation Tasks

1. **Create transport module structure**
2. **Implement EnvelopeSerializer trait and RequestResponseEnvelope**
3. **Implement Sans I/O protocol state machine**
4. **Create NetTransport trait and ServerTransport implementation**
5. **Implement I/O driver integration**
6. **Update PingPongServerActor to use NetTransport**
7. **Validate existing single-server test passes with NetTransport**
8. **Add sometimes assertions during implementation**:
   - Server scenarios (bind failures, envelope processing, response sending)
   - Transport scenarios (envelope serialization, queue behavior)
   - Connection patterns (accept handling, connection recovery)
9. **Add strategic buggify points**:
   - Envelope corruption
   - Bind failures
   - Response dropping
   - Connection disruption

## Success Criteria

- ✅ Single server test passes using NetTransport instead of direct bind/accept
- ✅ Server receives PING messages via envelope serialization
- ✅ Server sends PONG responses using transport abstraction
- ✅ Client continues using Peer (unchanged) and successfully communicates
- ✅ Protocol state machine testable without I/O
- ✅ All existing chaos testing and assertions continue to work
- ✅ Foundation established for future multi-server scenarios

This phase establishes the NetTransport abstraction and Sans I/O foundation while maintaining compatibility with existing tests.