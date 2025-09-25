# Phase 2e: Ping-Pong Integration Testing

## Overview

Phase 2e implements comprehensive ping-pong communication tests that demonstrate the full capabilities of the simulation framework. This phase validates that both `SimNetworkProvider` and `TokioNetworkProvider` can handle complex bidirectional communication patterns with the same application code.

## Prerequisites

Phase 2d must be completed:
- ✅ Sleep functionality and waker system implemented
- ✅ Real event processing drives simulation behavior  
- ✅ Futures properly coordinate with event system
- ✅ All existing network operations work correctly

## Target: Bidirectional Communication Testing

### What is Ping-Pong Testing?
Ping-pong tests involve bidirectional message exchange between client and server, where each side responds to messages from the other. This pattern validates:
- **Bidirectional data flow**: Both reading and writing work correctly
- **Async coordination**: Proper handling of concurrent reads and writes  
- **Buffer management**: Data integrity through multiple round trips
- **Event sequencing**: Correct ordering of network events
- **Provider equivalence**: Identical behavior with simulation and real networking

### Target Test Pattern
```rust
// This exact code works with both providers
async fn ping_pong_session<P>(provider: P, rounds: usize) -> io::Result<Vec<String>>
where P: NetworkProvider 
{
    let listener = provider.bind("ping-pong-server").await?;
    let mut client = provider.connect("ping-pong-server").await?;
    let (mut server, _) = listener.accept().await?;
    
    let mut responses = Vec::new();
    
    for i in 0..rounds {
        let ping_msg = format!("PING-{}", i);
        
        // Client sends ping
        client.write_all(ping_msg.as_bytes()).await?;
        
        // Server receives ping  
        let mut buf = [0; 1024];
        let n = server.read(&mut buf).await?;
        let received = String::from_utf8_lossy(&buf[..n]);
        
        // Server responds with pong
        let pong_msg = format!("PONG-{}", i);
        server.write_all(pong_msg.as_bytes()).await?;
        
        // Client receives pong
        let n = client.read(&mut buf).await?;
        let response = String::from_utf8_lossy(&buf[..n]).to_string();
        responses.push(response);
    }
    
    Ok(responses)
}

// Test with both providers
let sim_responses = ping_pong_session(sim_provider, 5).await?;
let tokio_responses = ping_pong_session(tokio_provider, 5).await?;
assert_eq!(sim_responses, tokio_responses); // Identical behavior
```

## Implementation Plan

### 1. Enhanced Connection Management

**File: `moonpool-foundation/src/network/sim/stream.rs`** (enhancements)

The current implementation needs improvements for reliable bidirectional communication:

```rust
impl AsyncRead for SimTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let sim = self.sim.upgrade()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown"))?;

        // Try to read from connection's receive buffer
        let mut temp_buf = vec![0u8; buf.remaining()];
        let bytes_read = sim
            .read_from_connection(self.connection_id, &mut temp_buf)
            .map_err(|e| io::Error::other(format!("read error: {}", e)))?;

        if bytes_read > 0 {
            buf.put_slice(&temp_buf[..bytes_read]);
            Poll::Ready(Ok(()))
        } else {
            // No data available - register for notification when data arrives
            sim.register_read_waker(self.connection_id, cx.waker().clone())
                .map_err(|e| io::Error::other(format!("waker registration error: {}", e)))?;
            Poll::Pending
        }
    }
}
```

### 2. Connection Pairing for Bidirectional Communication  

**File: `moonpool-foundation/src/network/sim/types.rs`** (additions)

```rust
/// Represents a paired connection for bidirectional communication
#[derive(Debug)]
pub(crate) struct ConnectionPair {
    pub client_id: ConnectionId,
    pub server_id: ConnectionId,
}

/// Enhanced connection state with bidirectional support
#[derive(Debug)]
pub(crate) struct ConnectionState {
    pub id: ConnectionId,
    pub addr: String,
    pub receive_buffer: VecDeque<u8>,
    pub paired_connection: Option<ConnectionId>, // For bidirectional communication
}
```

### 3. SimWorld Connection Pairing

**File: `moonpool-foundation/src/sim.rs`** (additions)

```rust
impl SimWorld {
    /// Create a connection pair for bidirectional communication
    pub(crate) fn create_connection_pair(&self, client_addr: String, server_addr: String) -> SimulationResult<ConnectionPair> {
        let mut inner = self.inner.borrow_mut();
        
        let client_id = ConnectionId(inner.next_connection_id);
        inner.next_connection_id += 1;
        
        let server_id = ConnectionId(inner.next_connection_id); 
        inner.next_connection_id += 1;
        
        // Create paired connections
        inner.connections.insert(client_id, ConnectionState {
            id: client_id,
            addr: client_addr,
            receive_buffer: VecDeque::new(),
            paired_connection: Some(server_id),
        });
        
        inner.connections.insert(server_id, ConnectionState {
            id: server_id,
            addr: server_addr, 
            receive_buffer: VecDeque::new(),
            paired_connection: Some(client_id),
        });
        
        Ok(ConnectionPair { client_id, server_id })
    }
    
    /// Write data to paired connection (for bidirectional communication)
    pub(crate) fn write_to_paired_connection(
        &self,
        from_connection: ConnectionId,
        data: &[u8],
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        
        // Find the paired connection
        let paired_id = inner.connections
            .get(&from_connection)
            .and_then(|conn| conn.paired_connection)
            .ok_or_else(|| SimulationError::InvalidState("no paired connection".to_string()))?;
        
        // Write data to paired connection's receive buffer
        if let Some(paired_conn) = inner.connections.get_mut(&paired_id) {
            for &byte in data {
                paired_conn.receive_buffer.push_back(byte);
            }
            
            // Wake any futures waiting to read from paired connection
            if let Some(waker) = inner.read_wakers.remove(&paired_id) {
                waker.wake();
            }
            
            Ok(())
        } else {
            Err(SimulationError::InvalidState("paired connection not found".to_string()))
        }
    }
    
    /// Register a waker for read operations
    pub(crate) fn register_read_waker(&self, connection_id: ConnectionId, waker: Waker) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.read_wakers.insert(connection_id, waker);
        Ok(())
    }
}
```

### 4. Enhanced Write Implementation  

**File: `moonpool-foundation/src/network/sim/stream.rs`** (updated AsyncWrite)

```rust
impl AsyncWrite for SimTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let sim = self.sim.upgrade()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "simulation shutdown"))?;

        // Get write delay from network configuration
        let delay = sim.with_network_config_and_rng(|config, rng| 
            config.latency.write_latency.sample(rng));

        // Schedule data delivery event to paired connection
        sim.schedule_event(
            Event::DataDelivery {
                connection_id: self.connection_id.0,
                data: buf.to_vec(),
            },
            delay,
        );

        Poll::Ready(Ok(buf.len()))
    }
}
```

### 5. Enhanced Event Processing

**File: `moonpool-foundation/src/sim.rs`** (updated event processing)

```rust
fn process_event_with_inner(inner: &mut SimInner, event: Event) {
    match event {
        Event::DataDelivery { connection_id, data } => {
            let connection_id = ConnectionId(connection_id);
            
            // Write data to the paired connection's receive buffer
            if let Some(connection) = inner.connections.get(&connection_id) {
                if let Some(paired_id) = connection.paired_connection {
                    if let Some(paired_conn) = inner.connections.get_mut(&paired_id) {
                        for &byte in &data {
                            paired_conn.receive_buffer.push_back(byte);
                        }
                        
                        // Wake any futures waiting to read from paired connection  
                        if let Some(waker) = inner.read_wakers.remove(&paired_id) {
                            waker.wake();
                        }
                    }
                }
            }
        }
        
        // Handle other events...
        Event::ConnectionReady { connection_id } => {
            // Connection establishment completed
            let connection_id = ConnectionId(connection_id);
            inner.ready_connections.insert(connection_id);
            
            if let Some(waker) = inner.connection_wakers.remove(&connection_id) {
                waker.wake();
            }
        }
        
        // ... other event handling
    }
}
```

### 6. Updated Provider Implementation

**File: `moonpool-foundation/src/network/sim/provider.rs`** (enhanced connect method)

```rust
#[async_trait(?Send)]
impl NetworkProvider for SimNetworkProvider {
    async fn connect(&self, addr: &str) -> io::Result<Self::TcpStream> {
        let sim = self.sim.upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // Get connect delay and create connection
        let delay = sim.with_network_config_and_rng(|config, rng| 
            config.latency.connect_latency.sample(rng));

        // For ping-pong, we need to find the corresponding listener
        // and create a proper connection pair
        let connection_pair = sim.create_connection_pair(
            "client-addr".to_string(),
            addr.to_string()
        ).map_err(|e| io::Error::other(format!("Failed to create connection pair: {}", e)))?;

        // Schedule connection ready event
        sim.schedule_event(
            Event::ConnectionReady { connection_id: connection_pair.client_id.0 },
            delay,
        );

        let stream = SimTcpStream::new(self.sim.clone(), connection_pair.client_id);
        Ok(stream)
    }
}
```

## Testing Strategy

### Core Ping-Pong Tests

**File: `moonpool-foundation/tests/ping_pong_tests.rs`** (new)

```rust
use moonpool_simulation::{NetworkConfiguration, NetworkProvider, SimWorld, TokioNetworkProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::io;

/// Generic ping-pong test that works with any NetworkProvider
async fn ping_pong_session<P>(provider: P, rounds: usize) -> io::Result<Vec<String>>
where 
    P: NetworkProvider + Clone
{
    let listener = provider.bind("ping-pong-server").await?;
    let mut client = provider.connect("ping-pong-server").await?;
    
    // Accept server connection  
    let (mut server, _peer_addr) = listener.accept().await?;
    
    let mut responses = Vec::new();
    
    for i in 0..rounds {
        let ping_msg = format!("PING-{}", i);
        
        // Client sends ping
        client.write_all(ping_msg.as_bytes()).await?;
        
        // Server receives ping
        let mut buf = [0; 1024];
        let n = server.read(&mut buf).await?;
        let received_ping = String::from_utf8_lossy(&buf[..n]);
        
        // Verify ping message
        assert_eq!(received_ping, ping_msg);
        
        // Server responds with pong
        let pong_msg = format!("PONG-{}", i);
        server.write_all(pong_msg.as_bytes()).await?;
        
        // Client receives pong
        let n = client.read(&mut buf).await?;
        let response = String::from_utf8_lossy(&buf[..n]).to_string();
        responses.push(response);
    }
    
    Ok(responses)
}

#[test]
fn test_simulation_ping_pong() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();
        
        let responses = ping_pong_session(provider, 5).await.unwrap();
        
        // Process all simulation events
        sim.run_until_empty();
        
        // Verify responses
        let expected: Vec<String> = (0..5).map(|i| format!("PONG-{}", i)).collect();
        assert_eq!(responses, expected);
        
        // Verify simulation time advanced
        assert!(sim.current_time() > std::time::Duration::ZERO);
        
        println!("Simulation ping-pong completed in {:?}", sim.current_time());
        println!("Responses: {:?}", responses);
    });
}

#[test]
fn test_tokio_ping_pong() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let provider = TokioNetworkProvider::new();
        
        let responses = ping_pong_session(provider, 5).await.unwrap();
        
        // Verify responses
        let expected: Vec<String> = (0..5).map(|i| format!("PONG-{}", i)).collect();
        assert_eq!(responses, expected);
        
        println!("Tokio ping-pong completed successfully");
        println!("Responses: {:?}", responses);
    });
}

#[test] 
fn test_provider_equivalence() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Test with simulation provider
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let sim_provider = sim.network_provider();
        let sim_responses = ping_pong_session(sim_provider, 3).await.unwrap();
        sim.run_until_empty();
        
        // Test with Tokio provider  
        let tokio_provider = TokioNetworkProvider::new();
        let tokio_responses = ping_pong_session(tokio_provider, 3).await.unwrap();
        
        // Both providers should produce identical results
        assert_eq!(sim_responses, tokio_responses);
        
        println!("Provider equivalence verified!");
        println!("Simulation responses: {:?}", sim_responses);
        println!("Tokio responses: {:?}", tokio_responses);
    });
}

#[test]
fn test_extended_ping_pong_session() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Test longer session with configurable latency
        let config = NetworkConfiguration::wan_simulation(); // Higher latency
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();
        
        let rounds = 10;
        let responses = ping_pong_session(provider, rounds).await.unwrap();
        sim.run_until_empty();
        
        // Verify all responses received correctly
        assert_eq!(responses.len(), rounds);
        for (i, response) in responses.iter().enumerate() {
            assert_eq!(response, &format!("PONG-{}", i));
        }
        
        // With WAN simulation config, should take significant time
        assert!(sim.current_time().as_millis() > 50);
        
        println!("Extended session ({} rounds) completed in {:?}", rounds, sim.current_time());
    });
}

#[test]
fn test_concurrent_ping_pong() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();
        
        // Test multiple concurrent ping-pong sessions
        let provider1 = provider.clone();
        let provider2 = provider.clone();
        
        let session1 = ping_pong_session(provider1, 3);
        let session2 = ping_pong_session(provider2, 3);
        
        // Run both sessions concurrently
        let (responses1, responses2) = tokio::try_join!(session1, session2).unwrap();
        
        sim.run_until_empty();
        
        // Both sessions should succeed
        let expected: Vec<String> = (0..3).map(|i| format!("PONG-{}", i)).collect();
        assert_eq!(responses1, expected);
        assert_eq!(responses2, expected);
        
        println!("Concurrent ping-pong sessions completed successfully");
    });
}
```

### Data Integrity Tests

**File: `moonpool-foundation/tests/ping_pong_tests.rs`** (additions)

```rust
#[test]
fn test_large_message_ping_pong() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();
        
        async fn large_message_ping_pong<P>(provider: P) -> io::Result<bool>
        where P: NetworkProvider
        {
            let listener = provider.bind("large-message-server").await?;
            let mut client = provider.connect("large-message-server").await?;
            let (mut server, _) = listener.accept().await?;
            
            // Send a large message (1KB)
            let large_data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
            client.write_all(&large_data).await?;
            
            // Server receives and echoes back
            let mut received = Vec::new();
            let mut buf = [0; 256]; // Smaller buffer to test partial reads
            
            while received.len() < 1024 {
                let n = server.read(&mut buf).await?;
                if n == 0 { break; }
                received.extend_from_slice(&buf[..n]);
            }
            
            server.write_all(&received).await?;
            
            // Client receives echo
            let mut echo_received = Vec::new();
            while echo_received.len() < 1024 {
                let n = client.read(&mut buf).await?;
                if n == 0 { break; }
                echo_received.extend_from_slice(&buf[..n]);
            }
            
            Ok(large_data == echo_received)
        }
        
        let integrity_ok = large_message_ping_pong(provider).await.unwrap();
        sim.run_until_empty();
        
        assert!(integrity_ok, "Large message data integrity failed");
        
        println!("Large message ping-pong integrity test passed");
        println!("Simulation time: {:?}", sim.current_time());
    });
}

#[test]
fn test_deterministic_ping_pong() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let mut execution_times = Vec::new();
        let mut all_responses = Vec::new();
        
        // Run simulation multiple times
        for _run in 0..3 {
            let config = NetworkConfiguration::wan_simulation();
            let mut sim = SimWorld::new_with_network_config(config);
            let provider = sim.network_provider();
            
            let responses = ping_pong_session(provider, 3).await.unwrap();
            sim.run_until_empty();
            
            execution_times.push(sim.current_time());
            all_responses.push(responses);
        }
        
        // All runs should be identical
        let first_time = execution_times[0];
        let first_responses = &all_responses[0];
        
        for (i, (&time, responses)) in execution_times.iter().zip(all_responses.iter()).enumerate() {
            assert_eq!(time, first_time, "Run {} had different execution time", i);
            assert_eq!(responses, first_responses, "Run {} had different responses", i);
        }
        
        println!("Deterministic ping-pong verified across {} runs", execution_times.len());
        println!("Execution time: {:?}", first_time);
    });
}
```

## Library Integration

**File: `moonpool-foundation/src/lib.rs`** (no changes needed)

The existing public API already supports all ping-pong functionality through the existing traits.

## Phase 2e Success Criteria

- ✅ **All quality gates pass** (fmt, clippy, test)
- ✅ **Bidirectional communication works** with both SimNetworkProvider and TokioNetworkProvider
- ✅ **Identical behavior** between simulation and real networking
- ✅ **Data integrity maintained** through multiple round trips
- ✅ **Connection pairing** enables proper bidirectional data flow
- ✅ **Concurrent sessions** work correctly in simulation
- ✅ **Large message handling** tests buffer management
- ✅ **Deterministic behavior** with reproducible results
- ✅ **Performance characteristics** demonstrate realistic timing
- ✅ **All existing tests continue to pass**

## Key Features Demonstrated

### Bidirectional Data Flow
- Data written to one stream appears on the paired stream
- Both directions work simultaneously
- Buffer management handles partial reads/writes

### Provider Equivalence  
- Same application code works with both providers
- Identical results and behavior patterns
- Seamless swapping between simulation and real networking

### Complex Communication Patterns
- Multiple round trips in a single session
- Concurrent sessions with proper isolation
- Large message handling with data integrity

### Simulation Fidelity
- Realistic timing with configurable latency
- Deterministic behavior for reproducible testing
- Event-driven processing maintains correctness

## What Phase 2e Validates

1. **Network Simulation Framework Completeness**: All major networking operations work correctly
2. **Trait Design Quality**: Same code works seamlessly with different providers  
3. **Event System Robustness**: Complex communication patterns handled correctly
4. **Data Integrity**: Multiple round trips maintain data correctness
5. **Performance Modeling**: Realistic timing for testing distributed systems
6. **Deterministic Testing**: Reproducible results for reliable testing

Phase 2e demonstrates that the simulation framework is production-ready for testing distributed systems with complex networking requirements. The ping-pong tests validate that applications can reliably test their networking logic using simulation before deploying with real networking.