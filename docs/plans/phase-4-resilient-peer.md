# Phase 4: Resilient Peer Implementation

## Overview

Phase 4 implements a resilient Peer abstraction similar to FoundationDB's Peer class, but designed to work seamlessly with our NetworkProvider trait system. This enables automatic reconnection, fault tolerance, and message reliability while maintaining the ability to swap between simulation and real networking implementations.

## Motivation

Before injecting more advanced fault scenarios into our network simulation, we need robust connection management that can handle:
- Connection failures and automatic reconnection
- Message queuing during disconnections  
- Backpressure and flow control
- Connection lifecycle management
- Transparent failover between network providers

## FoundationDB Peer Analysis

From studying the FoundationDB FlowTransport.h header and implementation, the Peer class provides:

### Core Responsibilities
- **Connection Management**: Maintains connection state and handles reconnection logic
- **Message Queuing**: Buffers unsent and reliable packets during disconnections
- **Fault Tolerance**: Automatic reconnection with exponential backoff
- **Metrics Collection**: Tracks bytes sent/received, latency, connection counts
- **Protocol Negotiation**: Handles version compatibility between peers
- **Flow Control**: Manages backpressure and connection idle states

### Key Data Structures
```cpp
struct Peer : public ReferenceCounted<Peer> {
    TransportData* transport;
    NetworkAddress destination;
    UnsentPacketQueue unsent;          // Messages waiting to be sent
    ReliablePacketList reliable;       // Messages requiring delivery confirmation
    AsyncTrigger dataToSend;           // Signal when data becomes available
    Future<Void> connect;              // Connection establishment future
    AsyncTrigger resetPing;            // Reset ping timers
    AsyncTrigger resetConnection;      // Force connection reset
    bool compatible;                   // Protocol compatibility
    bool connected;                    // Current connection state
    bool outgoingConnectionIdle;       // Connection idle management
    double lastConnectTime;            // Last connection attempt
    double reconnectionDelay;          // Exponential backoff delay
    int64_t bytesReceived;             // Metrics
    int64_t bytesSent;                 // Metrics
    // ... additional metrics and state
};
```

### Connection Lifecycle
1. **Creation**: Peer created for destination address
2. **Connection**: Async connection establishment with connectionKeeper actor
3. **Communication**: Message sending/receiving with queuing
4. **Monitoring**: Ping/pong heartbeat and failure detection
5. **Reconnection**: Automatic reconnection on failure with backoff
6. **Cleanup**: Resource cleanup when peer no longer referenced

## Moonpool Peer Design

Our Peer implementation must integrate with the existing NetworkProvider trait system while providing similar functionality.

### Core Architecture

```rust
/// A resilient peer that manages connections to a remote address.
/// 
/// Provides automatic reconnection and message queuing while abstracting
/// over NetworkProvider implementations.
pub struct Peer<P: NetworkProvider> {
    /// Network provider for creating connections
    provider: P,
    
    /// Destination address
    destination: String,
    
    /// Current connection state
    connection: Option<P::TcpStream>,
    
    /// Message queue for pending sends
    send_queue: VecDeque<Vec<u8>>,
    
    /// Reconnection state
    reconnect_state: ReconnectState,
    
    /// Configuration for behavior
    config: PeerConfig,
}

/// Configuration for peer behavior
#[derive(Clone, Debug)]
pub struct PeerConfig {
    /// Initial reconnection delay
    pub initial_reconnect_delay: Duration,
    
    /// Maximum reconnection delay
    pub max_reconnect_delay: Duration,
    
    /// Maximum message queue size
    pub max_queue_size: usize,
    
    /// Connection timeout
    pub connection_timeout: Duration,
}

/// State for managing reconnections
#[derive(Debug)]
struct ReconnectState {
    /// Current backoff delay
    current_delay: Duration,
    
    /// Number of consecutive failures
    failure_count: u32,
    
    /// Last connection attempt time
    last_attempt: Option<std::time::Instant>,
}
```

### Key Methods

```rust
impl<P: NetworkProvider> Peer<P> {
    /// Create a new peer for the destination address
    pub fn new(provider: P, destination: String, config: PeerConfig) -> Self;
    
    /// Send data to the peer (queues if disconnected, attempts reconnect on-demand)
    pub async fn send(&mut self, data: Vec<u8>) -> Result<(), PeerError>;
    
    /// Receive data from the peer (attempts reconnect if needed)
    pub async fn receive(&mut self, buf: &mut [u8]) -> Result<usize, PeerError>;
    
    /// Check if currently connected
    pub fn is_connected(&self) -> bool;
    
    /// Force reconnection (drops current connection and reconnects)
    pub async fn reconnect(&mut self) -> Result<(), PeerError>;
    
    /// Close the connection and clear send queue
    pub async fn close(&mut self);
}
```

### Provider Integration

The Peer works with any NetworkProvider implementation:

```rust
// Using with simulation
let sim_provider = SimNetworkProvider::new(sim_world.downgrade());
let mut peer = Peer::new(sim_provider, "10.0.0.1:8080".to_string(), PeerConfig::default());

// Using with real networking  
let tokio_provider = TokioNetworkProvider::new();
let mut peer = Peer::new(tokio_provider, "10.0.0.1:8080".to_string(), PeerConfig::default());
```

### Message Queue Management

```rust
impl<P: NetworkProvider> Peer<P> {
    /// Add message to send queue
    fn queue_message(&mut self, data: Vec<u8>) -> Result<(), PeerError> {
        if self.send_queue.len() >= self.config.max_queue_size {
            // Drop oldest message (FIFO)
            self.send_queue.pop_front();
        }
        
        self.send_queue.push_back(data);
        Ok(())
    }
    
    /// Process queued messages when connection available
    async fn process_send_queue(&mut self) -> Result<(), PeerError> {
        while let Some(data) = self.send_queue.pop_front() {
            match self.send_raw(&data).await {
                Ok(_) => {
                    // Message sent successfully
                }
                Err(_) => {
                    // Connection failed - put message back and fail
                    self.send_queue.push_front(data);
                    return Err(PeerError::ConnectionFailed);
                }
            }
        }
        Ok(())
    }
}
```

### Reconnection Logic

```rust
impl<P: NetworkProvider> Peer<P> {
    /// Attempt to establish connection
    async fn connect(&mut self) -> Result<(), PeerError> {
        let delay = self.reconnect_state.current_delay;
        
        // Wait for backoff delay
        if let Some(last_attempt) = self.reconnect_state.last_attempt {
            let elapsed = last_attempt.elapsed();
            if elapsed < delay {
                // In simulation, use sim sleep; in real networking, use tokio sleep
                self.sleep(delay - elapsed).await;
            }
        }
        
        self.reconnect_state.last_attempt = Some(std::time::Instant::now());
        
        match timeout(self.config.connection_timeout, self.provider.connect(&self.destination)).await {
            Ok(Ok(stream)) => {
                self.connection = Some(stream);
                self.reconnect_state.failure_count = 0;
                self.reconnect_state.current_delay = self.config.initial_reconnect_delay;
                self.reconnect_state.reconnecting = false;
                self.metrics.connections_established += 1;
                self.metrics.last_connected = Some(std::time::Instant::now());
                Ok(())
            }
            Ok(Err(e)) | Err(_) => {
                self.handle_connection_failure();
                Err(PeerError::ConnectionFailed)
            }
        }
    }
    
    /// Handle connection failure with exponential backoff
    fn handle_connection_failure(&mut self) {
        self.connection = None;
        self.reconnect_state.failure_count += 1;
        self.reconnect_state.current_delay = std::cmp::min(
            self.reconnect_state.current_delay * 2,
            self.config.max_reconnect_delay
        );
        self.reconnect_state.reconnecting = true;
        self.metrics.connection_failures += 1;
    }
}
```

## Implementation Plan

### Phase 4a: Cleanup and Preparation
- ✅ Remove custom_metrics from SimulationMetrics (not needed for Phase 4)
- ✅ Rename report.rs to runner.rs or simulation_runner.rs (more descriptive name)
- ✅ Replace workload function signature with topology information:
  * Change from `|seed, provider, ip: Option<String>|` 
  * To `|seed, provider, topology: WorkloadTopology|`
  * Where `WorkloadTopology` contains `my_ip: String` and `peer_ips: Vec<String>`
  * This allows workloads to know about all other peers in the simulation

### Phase 4b: Basic Peer Structure
- ✅ Define Peer struct with generic NetworkProvider
- ✅ Implement basic connection management
- ✅ Add message queuing infrastructure
- ✅ Create PeerConfig and PeerMetrics

### Phase 4c: Reconnection Logic  
- ✅ Implement exponential backoff reconnection
- ✅ Add connection failure handling
- ✅ Integrate with both SimNetworkProvider and TokioNetworkProvider
- ✅ Add comprehensive error handling

### Phase 4d: Message Queueing
- ✅ Implement simple message queuing during disconnections
- ✅ Handle queue overflow with FIFO dropping
- ✅ Message ordering (FIFO send queue)

### Phase 4e: Integration and Polish
- ✅ Integrate with simulation time vs real time appropriately
- ✅ Add proper error handling and edge cases

### Phase 4f: Testing and Integration
- ✅ Comprehensive unit tests for all Peer functionality
- ✅ Integration tests with both network providers
- ✅ Modify ping_pong tests to use Peer instead of direct NetworkProvider
- ✅ Extend ping_pong test to send many more messages per iteration to increase chance of hitting reconnection scenarios
- ✅ Run ping_pong tests with many iterations to discover edge cases and strange behavior
- ✅ Add random connection closure during tests to trigger reconnection scenarios
- ✅ Use sometimes_assert! to verify we're exploring reconnection states (e.g., "connections should sometimes fail and reconnect")
- ✅ Ensure SimulationBuilder checks sometimes_assert! results at the end to validate test coverage
- ✅ Performance and load testing
- ✅ Example applications demonstrating usage
- ✅ Determine minimum number of iterations needed to hit all sometimes_assert! conditions with high confidence in ping_pong test

## API Design Goals

### 1. **Provider Agnostic**
```rust
// Same code works with simulation or real networking
async fn example_usage<P: NetworkProvider>(provider: P) -> Result<(), Box<dyn Error>> {
    let mut peer = Peer::new(provider, "127.0.0.1:8080".to_string(), PeerConfig::default());
    
    peer.send(b"hello".to_vec(), true).await?;
    
    let mut buf = [0; 1024];
    let n = peer.receive(&mut buf).await?;
    println!("Received: {:?}", &buf[..n]);
    
    Ok(())
}
```

### 2. **Automatic Fault Tolerance**
```rust
// Peer handles reconnection transparently
let mut peer = Peer::new(provider, "remote:8080".to_string(), config);

// Messages are queued during disconnections
peer.send(b"message1".to_vec()).await?; // May queue if disconnected
peer.send(b"message2".to_vec()).await?; // Will attempt reconnect and send

// Connection failures are handled transparently
// Queued messages are delivered once reconnected
```

### 3. **Simple Status**
```rust
// Just check if connected
if peer.is_connected() {
    println!("Peer is connected");
} else {
    println!("Peer is disconnected, {} messages queued", peer.queue_size());
}
```

### 4. **Configurable Behavior**
```rust
let config = PeerConfig {
    initial_reconnect_delay: Duration::from_millis(100),
    max_reconnect_delay: Duration::from_secs(30),
    max_queue_size: 1000,
    connection_timeout: Duration::from_secs(5),
};
```

## Integration with Existing Framework

### SimNetworkProvider Integration
The Peer will work seamlessly with our simulation framework:
- Uses simulation time for delays and timeouts
- Respects network fault injection configurations
- Participates in deterministic event ordering
- Provides metrics for simulation reports

### TokioNetworkProvider Integration
The Peer will work with real networking:
- Uses actual wall-clock time for delays
- Handles real network failures and conditions
- Provides production-ready connection management
- Enables development and testing on real networks

## Success Criteria

- ✅ Peer works identically with both SimNetworkProvider and TokioNetworkProvider
- ✅ Automatic reconnection with configurable backoff strategies  
- ✅ Simple message queuing during disconnections
- ✅ Fault tolerance under various network conditions
- ✅ Clean integration with existing simulation framework
- ✅ Easy-to-use API that abstracts complexity
- ✅ No background tasks - all operations on-demand

## Future Extensions (Post Phase 4)

- **Connection Pooling**: Multiple connections per peer for higher throughput
- **Load Balancing**: Distribution across multiple destination addresses  
- **Protocol Negotiation**: Version compatibility handling
- **Encryption**: TLS/SSL support for secure connections
- **Compression**: Optional message compression for bandwidth optimization
- **Flow Control**: Backpressure mechanisms for congestion management

This Phase 4 implementation will provide the robust foundation needed for advanced fault injection testing while maintaining our core principle of seamless provider swapping between simulation and production environments.