# Phase 2c: Configurable Network Latency

## Overview

Phase 2c enhances the simulation framework from Phase 2b by adding configurable network latency and removing hardcoded test data. The goal is to provide fine-grained control over network simulation parameters while ensuring proper simulation time advancement.

## Prerequisites

Phase 2b must be completed:
- ✅ SimNetworkProvider implements NetworkProvider trait
- ✅ Basic simulation with hardcoded delays working
- ✅ Echo server tests passing with simulation
- ✅ Event scheduling and time advancement working

## Key Improvements in Phase 2c

### 1. Remove Hardcoded Test Data
Phase 2b had hardcoded test data (`b"Hello, echo server!"`) in the AsyncRead implementation. Phase 2c implements proper data flow through connection buffers.

### 2. Configurable Network Latency
Replace fixed delays with configurable latency ranges including jitter for realistic network simulation.

### 3. Proper Simulation Time Advancement
Fix simulation time advancement by replacing `tokio::time::sleep` with proper event scheduling.

## Implementation Plan

### 1. Network Configuration Structure

**File: `moonpool-foundation/src/network/config.rs`**

```rust
use std::time::Duration;

/// Configuration for network simulation parameters
#[derive(Debug, Clone, Default)]
pub struct NetworkConfiguration {
    /// Latency configuration for various network operations
    pub latency: LatencyConfiguration,
}

/// Configuration for network operation latencies
#[derive(Debug, Clone)]
pub struct LatencyConfiguration {
    /// Base latency and jitter range for bind operations
    pub bind_latency: LatencyRange,
    /// Base latency and jitter range for accept operations
    pub accept_latency: LatencyRange,
    /// Base latency and jitter range for connect operations
    pub connect_latency: LatencyRange,
    /// Base latency and jitter range for read operations
    pub read_latency: LatencyRange,
    /// Base latency and jitter range for write operations
    pub write_latency: LatencyRange,
}

/// Range specification for latency with base duration and jitter
#[derive(Debug, Clone)]
pub struct LatencyRange {
    /// Base latency duration
    pub base: Duration,
    /// Maximum additional jitter duration (0 to this value)
    pub jitter: Duration,
}

impl LatencyRange {
    /// Create a new latency range
    pub fn new(base: Duration, jitter: Duration) -> Self {
        Self { base, jitter }
    }

    /// Create a fixed latency with no jitter
    pub fn fixed(duration: Duration) -> Self {
        Self {
            base: duration,
            jitter: Duration::ZERO,
        }
    }

    /// Generate a random duration within this range using the provided RNG
    pub fn sample<R: rand::Rng>(&self, rng: &mut R) -> Duration {
        if self.jitter.is_zero() {
            self.base
        } else {
            let jitter_nanos = rng.gen_range(0..=self.jitter.as_nanos() as u64);
            self.base + Duration::from_nanos(jitter_nanos)
        }
    }
}

impl NetworkConfiguration {
    /// Create a configuration optimized for fast local testing
    pub fn fast_local() -> Self {
        Self {
            latency: LatencyConfiguration {
                bind_latency: LatencyRange::fixed(Duration::from_micros(1)),
                accept_latency: LatencyRange::fixed(Duration::from_micros(10)),
                connect_latency: LatencyRange::fixed(Duration::from_micros(10)),
                read_latency: LatencyRange::fixed(Duration::from_micros(1)),
                write_latency: LatencyRange::fixed(Duration::from_micros(1)),
            },
        }
    }

    /// Create a configuration simulating slower WAN conditions
    pub fn wan_simulation() -> Self {
        Self {
            latency: LatencyConfiguration {
                bind_latency: LatencyRange::new(Duration::from_millis(1), Duration::from_millis(2)),
                accept_latency: LatencyRange::new(
                    Duration::from_millis(10),
                    Duration::from_millis(20),
                ),
                connect_latency: LatencyRange::new(
                    Duration::from_millis(50),
                    Duration::from_millis(100),
                ),
                read_latency: LatencyRange::new(
                    Duration::from_millis(5),
                    Duration::from_millis(15),
                ),
                write_latency: LatencyRange::new(
                    Duration::from_millis(10),
                    Duration::from_millis(30),
                ),
            },
        }
    }
}
```

### 2. SimWorld Configuration Support

**File: `moonpool-foundation/src/sim.rs`** (additions)

```rust
struct SimInner {
    // Existing fields...
    
    // Phase 2c network configuration
    network_config: NetworkConfiguration,
}

impl SimWorld {
    /// Creates a new simulation world with custom network configuration.
    pub fn new_with_network_config(network_config: NetworkConfiguration) -> Self {
        Self {
            inner: Rc::new(RefCell::new(SimInner::new_with_config(network_config))),
        }
    }

    /// Access network configuration and RNG together for latency calculations
    pub fn with_network_config_and_rng<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&NetworkConfiguration, &mut ChaCha8Rng) -> R,
    {
        let mut inner = self.inner.borrow_mut();
        let config = &inner.network_config.clone();
        f(config, &mut inner.rng)
    }
}
```

### 3. Proper Data Flow Implementation

**File: `moonpool-foundation/src/network/sim/stream.rs`** (updates)

```rust
impl AsyncRead for SimTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let sim = self.sim.upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // Try to read from connection's receive buffer (no hardcoded data)
        let mut temp_buf = vec![0u8; buf.remaining()];
        let bytes_read = sim
            .read_from_connection(self.connection_id, &mut temp_buf)
            .map_err(|e| io::Error::other(format!("read error: {}", e)))?;

        if bytes_read > 0 {
            buf.put_slice(&temp_buf[..bytes_read]);
        }

        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for SimTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let sim = self.sim.upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // Get write delay from network configuration
        let delay = sim.with_network_config_and_rng(|config, rng| 
            config.latency.write_latency.sample(rng));

        // Immediately write data to the connection's buffer
        sim.write_to_connection(self.connection_id, buf)
            .map_err(|e| io::Error::other(format!("write error: {}", e)))?;

        // Schedule write completion event with configured delay
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

### 4. Fixed Simulation Time Advancement

**File: `moonpool-foundation/src/network/sim/provider.rs`** (updates)

```rust
#[async_trait(?Send)]
impl NetworkProvider for SimNetworkProvider {
    async fn bind(&self, addr: &str) -> io::Result<Self::TcpListener> {
        let sim = self.sim.upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // Get bind delay from network configuration and schedule bind completion event
        let delay = sim.with_network_config_and_rng(|config, rng| 
            config.latency.bind_latency.sample(rng));

        let listener_id = sim.create_listener(addr.to_string())
            .map_err(|e| io::Error::other(format!("Failed to create listener: {}", e)))?;

        // Schedule event to advance simulation time (not tokio::time::sleep)
        sim.schedule_event(
            Event::BindComplete { listener_id: listener_id.0 },
            delay,
        );

        Ok(SimTcpListener::new(self.sim.clone(), listener_id, addr.to_string()))
    }
}
```

### 5. Data Buffer Management

**File: `moonpool-foundation/src/sim.rs`** (additions)

```rust
impl SimWorld {
    /// Read data from connection's receive buffer
    pub(crate) fn read_from_connection(
        &self,
        connection_id: ConnectionId,
        buf: &mut [u8],
    ) -> SimulationResult<usize> {
        let mut inner = self.inner.borrow_mut();

        if let Some(connection) = inner.connections.get_mut(&connection_id) {
            let mut bytes_read = 0;
            while bytes_read < buf.len() && !connection.receive_buffer.is_empty() {
                if let Some(byte) = connection.receive_buffer.pop_front() {
                    buf[bytes_read] = byte;
                    bytes_read += 1;
                }
            }
            Ok(bytes_read)
        } else {
            Err(SimulationError::InvalidState("connection not found".to_string()))
        }
    }

    /// Write data to connection's receive buffer
    pub(crate) fn write_to_connection(
        &self,
        connection_id: ConnectionId,
        data: &[u8],
    ) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();

        if let Some(connection) = inner.connections.get_mut(&connection_id) {
            for &byte in data {
                connection.receive_buffer.push_back(byte);
            }
            Ok(())
        } else {
            Err(SimulationError::InvalidState("connection not found".to_string()))
        }
    }
}
```

## Testing Strategy

### Configurable Latency Tests

**File: `moonpool-foundation/tests/configurable_latency.rs`**

```rust
use moonpool_simulation::{
    LatencyRange, NetworkConfiguration, NetworkProvider, SimWorld, TcpListenerTrait,
};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[test]
fn test_fast_local_configuration() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let fast_config = NetworkConfiguration::fast_local();
        let mut sim = SimWorld::new_with_network_config(fast_config);
        let provider = sim.network_provider();

        simple_network_test(provider, "fast-test").await.unwrap();
        sim.run_until_empty();
        let sim_time = sim.current_time();

        // Fast local config should complete quickly (less than 1ms simulation time)
        assert!(
            sim_time < Duration::from_millis(1),
            "Fast local should be under 1ms, got {:?}",
            sim_time
        );
    });
}

#[test]
fn test_wan_simulation_configuration() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let wan_config = NetworkConfiguration::wan_simulation();
        let mut sim = SimWorld::new_with_network_config(wan_config);
        let provider = sim.network_provider();

        simple_network_test(provider, "wan-test").await.unwrap();
        sim.run_until_empty();
        let sim_time = sim.current_time();

        // WAN config should take significantly longer (at least 10ms simulation time)
        assert!(
            sim_time > Duration::from_millis(10),
            "WAN simulation should be over 10ms, got {:?}",
            sim_time
        );
    });
}

#[test]
fn test_custom_latency_configuration() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Create custom configuration with specific latency ranges
        let mut config = NetworkConfiguration::default();
        config.latency.bind_latency = LatencyRange::fixed(Duration::from_millis(5));
        config.latency.accept_latency = LatencyRange::fixed(Duration::from_millis(10));
        config.latency.write_latency = LatencyRange::fixed(Duration::from_millis(2));

        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        simple_network_test(provider, "custom-test").await.unwrap();
        sim.run_until_empty();
        let sim_time = sim.current_time();

        // Configured delays should be reflected in simulation time
        assert!(
            sim_time >= Duration::from_millis(1),
            "Custom config should have at least 1ms latency, got {:?}",
            sim_time
        );
    });
}
```

### Simulation Time Advancement Test

**File: `moonpool-foundation/tests/simulation_integration.rs`** (updated)

```rust
#[test]
fn test_simple_echo_simulation() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        // Use a configuration with noticeable delays for this test
        let config = NetworkConfiguration::wan_simulation();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        simple_echo_server(provider, "echo-server").await.unwrap();
        sim.run_until_empty();

        // Verify time advanced due to simulated delays
        assert!(sim.current_time() > std::time::Duration::ZERO);
        println!("Simulation completed in {:?}", sim.current_time());
    });
}
```

## Library Integration

**File: `moonpool-foundation/src/lib.rs`** (updates)

```rust
// Network exports
pub use network::{
    LatencyConfiguration, LatencyRange, NetworkConfiguration, NetworkProvider,
    SimNetworkProvider, TcpListenerTrait, TokioNetworkProvider,
};
```

## Phase 2c Success Criteria

- ✅ All quality gates pass (fmt, clippy, test)
- ✅ Hardcoded test data removed from SimTcpStream
- ✅ NetworkConfiguration provides flexible latency control
- ✅ Simulation time advances properly with configured delays
- ✅ Data flows correctly through connection receive buffers
- ✅ Tests demonstrate fast_local, wan_simulation, and custom configurations
- ✅ Deterministic behavior maintained with configurable jitter
- ✅ All existing tests continue to pass

## Key Differences from Phase 2b

### What Was Fixed
- **Simulation Time**: Replaced `tokio::time::sleep` with proper event scheduling
- **Data Flow**: Removed hardcoded test data, implemented proper buffering
- **Configuration**: Added flexible latency configuration with jitter support

### What Was Added
- **NetworkConfiguration**: Complete configuration system
- **LatencyRange**: Base + jitter latency specification
- **Buffer Management**: Proper data flow through connection buffers
- **Configuration Tests**: Comprehensive test coverage for different scenarios

### What Was Preserved
- **Trait Compatibility**: Same NetworkProvider interface
- **Deterministic Behavior**: Reproducible results with same seed
- **Event Integration**: Proper integration with SimWorld event system
- **Seamless Swapping**: Application code unchanged

Phase 2c completes the configurable network simulation foundation, providing fine-grained control over network timing while maintaining the seamless trait-based design established in Phase 2a and 2b.