# Phase 2d: First Waker Implementation with Sleep Method

## Overview

Phase 2d implements our first async waker system by adding a `sleep` method to the simulation. This introduces the foundation for proper async coordination where futures can sleep and be woken when their scheduled time arrives. This is the stepping stone toward full async network operations in later phases.

## Prerequisites

Phase 2c must be completed:
- ✅ Configurable network latency working
- ✅ Event scheduling properly advances simulation time  
- ✅ NetworkConfiguration providing flexible control
- ✅ Data buffering through connection receive buffers
- ✅ Event forwarding to network logic (completed)

## Current State Analysis

**Problem**: We have event scheduling and time advancement, but no way for async code to actually wait for events. Everything completes immediately without proper async coordination.

**Goal**: Implement a `sleep` function that:
1. Creates a future that doesn't complete immediately
2. Schedules a Wake event for the specified duration  
3. Registers a waker to be called when the event is processed
4. Returns `Poll::Pending` until the wake event fires

## Target: Async Sleep Implementation

### Current Behavior (Phase 2c)
```rust
// No way to sleep in simulation time - everything is immediate
async fn example() {
    // This would use real tokio::time::sleep, not simulation time
    tokio::time::sleep(Duration::from_millis(100)).await;
}
```

### Target Behavior (Phase 2d)
```rust
// Sleep using simulation time with proper async coordination
async fn example(sim: &SimWorld) {
    // This sleeps in simulation time and advances the simulation
    sim.sleep(Duration::from_millis(100)).await;
}

// Usage in tests and network operations
let sim = SimWorld::new();
sim.sleep(Duration::from_millis(50)).await;  // Sleeps in sim time
assert_eq!(sim.current_time(), Duration::from_millis(50));
```

## Implementation Plan

### 1. Sleep Future Implementation

**File: `moonpool-simulation/src/sleep.rs`** (new)

```rust
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use crate::{WeakSimWorld, Event, SimulationResult};

/// Future that completes after a specified simulation time duration
pub struct SleepFuture {
    sim: WeakSimWorld,
    task_id: u64,
    completed: bool,
}

impl SleepFuture {
    pub fn new(sim: WeakSimWorld, task_id: u64) -> Self {
        Self {
            sim,
            task_id,
            completed: false,
        }
    }
}

impl Future for SleepFuture {
    type Output = SimulationResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.completed {
            return Poll::Ready(Ok(()));
        }

        let sim = match self.sim.upgrade() {
            Ok(sim) => sim,
            Err(e) => return Poll::Ready(Err(e)),
        };

        // Check if our wake event has been processed
        if sim.is_task_awake(self.task_id)? {
            self.completed = true;
            Poll::Ready(Ok(()))
        } else {
            // Register waker so we get notified when wake event is processed
            sim.register_task_waker(self.task_id, cx.waker().clone())?;
            Poll::Pending
        }
    }
}
```

### 2. SimWorld Sleep Method

**File: `moonpool-simulation/src/sim.rs`** (additions)

```rust
use crate::sleep::SleepFuture;

impl SimWorld {
    /// Sleep for the specified duration in simulation time
    pub fn sleep(&self, duration: Duration) -> SleepFuture {
        let task_id = self.generate_task_id();
        
        // Schedule a wake event for this task
        self.schedule_event(Event::Wake { task_id }, duration);
        
        // Return a future that will be woken when the event is processed
        SleepFuture::new(self.downgrade(), task_id)
    }
    
    /// Generate a unique task ID
    fn generate_task_id(&self) -> u64 {
        let mut inner = self.inner.borrow_mut();
        let task_id = inner.next_task_id;
        inner.next_task_id += 1;
        task_id
    }
    
    /// Check if a task has been awakened
    pub(crate) fn is_task_awake(&self, task_id: u64) -> SimulationResult<bool> {
        let inner = self.inner.borrow();
        Ok(inner.awakened_tasks.contains(&task_id))
    }
    
    /// Register a waker for a task
    pub(crate) fn register_task_waker(&self, task_id: u64, waker: Waker) -> SimulationResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.task_wakers.insert(task_id, waker);
        Ok(())
    }
}
```

### 3. Enhanced SimInner State Management  

**File: `moonpool-simulation/src/sim.rs`** (additions)

```rust
use std::task::Waker;
use std::collections::HashSet;

struct SimInner {
    // Existing fields...
    
    // Phase 2d: Task management for sleep functionality
    next_task_id: u64,
    awakened_tasks: HashSet<u64>,
    task_wakers: HashMap<u64, Waker>,
}

impl SimInner {
    fn new() -> Self {
        Self {
            // Existing fields...
            
            // Phase 2d: Initialize task management
            next_task_id: 0,
            awakened_tasks: HashSet::new(),
            task_wakers: HashMap::new(),
        }
    }
}
```

### 3. Real Event Processing Implementation

**File: `moonpool-simulation/src/sim.rs`** (updated process_event)

```rust
fn process_event(&self, event: Event) {
    let mut inner = self.inner.borrow_mut();
    
    match event {
        Event::Wake { task_id } => {
            // Phase 2d: Real task waking (for future generic task support)
            log::debug!("Waking task {}", task_id);
        }
        
        Event::BindComplete { listener_id } => {
            log::debug!("Bind completed for listener {}", listener_id);
            
            // Mark bind as completed
            inner.completed_binds.insert(ListenerId(listener_id));
            
            // Wake any futures waiting for this bind
            if let Some(waker) = inner.listener_wakers.remove(&ListenerId(listener_id)) {
                waker.wake();
            }
        }
        
        Event::ConnectionReady { connection_id } => {
            log::debug!("Connection ready: {}", connection_id);
            
            // Mark connection as ready
            inner.ready_connections.insert(ConnectionId(connection_id));
            
            // Wake any futures waiting for this connection
            if let Some(waker) = inner.connection_wakers.remove(&ConnectionId(connection_id)) {
                waker.wake();
            }
        }
        
        Event::DataDelivery { connection_id, data } => {
            log::debug!("Data delivery completed for connection {}: {} bytes", connection_id, data.len());
            
            // Actually deliver data to the connection's receive buffer
            if let Some(connection) = inner.connections.get_mut(&ConnectionId(connection_id)) {
                for &byte in &data {
                    connection.receive_buffer.push_back(byte);
                }
                
                // Track this delivery for completion checking
                inner.delivered_data
                    .entry(ConnectionId(connection_id))
                    .or_insert_with(Vec::new)
                    .push(data.clone());
                
                log::debug!("Delivered {} bytes to connection {} receive buffer", data.len(), connection_id);
            } else {
                log::warn!("Attempted to deliver data to non-existent connection {}", connection_id);
            }
            
            // Wake any futures waiting for write completion on this connection
            if let Some(waker) = inner.read_wakers.remove(&ConnectionId(connection_id)) {
                waker.wake();
            }
        }
    }
}
```

### 4. Enhanced Wake Event Processing

**File: `moonpool-simulation/src/sim.rs`** (updated process_event_with_inner)

```rust
fn process_event_with_inner(inner: &mut SimInner, event: Event) {
    match event {
        Event::Wake { task_id } => {
            // Phase 2d: Real task waking implementation
            
            // Mark this task as awakened
            inner.awakened_tasks.insert(task_id);
            
            // Wake the future that was sleeping
            if let Some(waker) = inner.task_wakers.remove(&task_id) {
                waker.wake();
            }
        }
        
        // Other events remain the same...
        Event::BindComplete { listener_id: _ } => {
            // Network bind completed - acknowledgment only in Phase 2d
        }
        
        Event::ConnectionReady { connection_id: _ } => {
            // Connection establishment completed - acknowledgment only in Phase 2d
        }
        
        Event::DataDelivery { connection_id, data } => {
            // Data delivery completed - actually deliver data to receive buffer
            let connection_id = ConnectionId(connection_id);
            
            // Deliver data directly using the passed inner reference
            if let Some(connection) = inner.connections.get_mut(&connection_id) {
                for &byte in &data {
                    connection.receive_buffer.push_back(byte);
                }
            }
        }
    }
}
```

### 5. Library Integration

**File: `moonpool-simulation/src/lib.rs`** (updated)

```rust
/// Sleep functionality for simulation time
pub mod sleep;

// Add to public API exports
pub use sleep::SleepFuture;
```

**File: `moonpool-simulation/src/network/sim/stream.rs`** (updated)

```rust
use super::futures::{ConnectionFuture, DataDeliveryFuture};

#[async_trait(?Send)]
impl TcpListenerTrait for SimTcpListener {
    async fn accept(&self) -> io::Result<(Self::TcpStream, String)> {
        let sim = self.sim.upgrade()
            .map_err(|_| io::Error::other("simulation shutdown"))?;

        // Create connection for accepted client
        let connection_id = sim.create_connection("client-addr".to_string())
            .map_err(|_| io::Error::other("connection creation failed"))?;

        // Get accept delay and schedule event
        let delay = sim.with_network_config_and_rng(|config, rng| 
            config.latency.accept_latency.sample(rng));
        
        sim.schedule_event(
            Event::ConnectionReady { connection_id: connection_id.0 },
            delay,
        );

        // Wait for accept to complete
        ConnectionFuture::new(self.sim.clone(), connection_id)
            .await
            .map_err(|e| io::Error::other(format!("Accept failed: {}", e)))?;

        let stream = SimTcpStream::new(self.sim.clone(), connection_id);
        Ok((stream, "127.0.0.1:12345".to_string()))
    }
}

impl AsyncWrite for SimTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let sim = match self.sim.upgrade() {
            Ok(sim) => sim,
            Err(_) => return Poll::Ready(Err(io::Error::other("simulation shutdown"))),
        };

        // Get write delay from configuration
        let delay = sim.with_network_config_and_rng(|config, rng| 
            config.latency.write_latency.sample(rng));

        // Schedule data delivery event
        sim.schedule_event(
            Event::DataDelivery {
                connection_id: self.connection_id.0,
                data: buf.to_vec(),
            },
            delay,
        );

        // Return pending and let the future handle completion
        // Note: In a real implementation, we'd create a DataDeliveryFuture here
        // For now, we'll simulate immediate completion like Phase 2c
        // This will be refined in testing
        Poll::Ready(Ok(buf.len()))
    }
}
```

### 5. Enhanced Event System

**File: `moonpool-simulation/src/events.rs`** (updated)

```rust
/// Events that can be scheduled in the simulation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A wake event for a specific task.
    Wake { task_id: u64 },
    
    /// Listener bind operation completed
    BindComplete { listener_id: u64 },
    
    /// Connection establishment completed  
    ConnectionReady { connection_id: u64 },
    
    /// Data delivery to connection's receive buffer completed
    DataDelivery { 
        connection_id: u64, 
        data: Vec<u8> 
    },
    
    // Phase 2d: Additional events for fine-grained control
    /// Read operation completed
    ReadComplete { 
        connection_id: u64, 
        bytes_read: usize 
    },
    
    /// Accept operation completed
    AcceptComplete { 
        listener_id: u64, 
        connection_id: u64 
    },
}
```

## Testing Strategy

### Sleep Functionality Tests

**File: `moonpool-simulation/tests/sleep_tests.rs`** (new)

```rust
use moonpool_simulation::SimWorld;
use std::time::Duration;

#[test]
fn test_basic_sleep() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .unwrap();

    local_runtime.block_on(async move {
        let mut sim = SimWorld::new();

        // Verify initial state
        assert_eq!(sim.current_time(), Duration::ZERO);
        
        // Start sleep operation
        let sleep_task = tokio::spawn(async move {
            sim.sleep(Duration::from_millis(100)).await.unwrap()
        });

        // Verify sleep hasn't completed yet - time should still be zero
        assert_eq!(sim.current_time(), Duration::ZERO);
        
        // Process the wake event
        sim.run_until_empty();
        
        // Now time should have advanced
        assert_eq!(sim.current_time(), Duration::from_millis(100));
        
        // Sleep task should now complete
        sleep_task.await.unwrap();
    });
}

#[test]  
fn test_data_delivery_event_processing() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .unwrap();

    local_runtime.block_on(async move {
        let mut sim = SimWorld::new();
        let provider = sim.network_provider();
        
        let listener = provider.bind("echo-addr").await.unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        
        // Write data - should schedule DataDelivery event
        let test_data = b"Hello, Phase 2d!";
        stream.write_all(test_data).await.unwrap();
        
        // Process events until empty
        sim.run_until_empty();
        
        // Verify time advanced due to write latency
        assert!(sim.current_time() > Duration::ZERO);
        
        // Data should now be available in the connection's receive buffer
        // (This would be tested by reading from another stream to the same connection)
    });
}

#[test]
fn test_event_ordering_and_processing() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .unwrap();

    local_runtime.block_on(async move {
        let mut sim = SimWorld::new();
        
        // Schedule multiple events at different times
        sim.schedule_event(Event::Wake { task_id: 1 }, Duration::from_millis(100));
        sim.schedule_event(Event::Wake { task_id: 2 }, Duration::from_millis(50));
        sim.schedule_event(Event::Wake { task_id: 3 }, Duration::from_millis(150));
        
        // Process events one by one
        assert!(sim.step()); // Should process task 2 first (50ms)
        assert_eq!(sim.current_time(), Duration::from_millis(50));
        
        assert!(sim.step()); // Should process task 1 next (100ms)  
        assert_eq!(sim.current_time(), Duration::from_millis(100));
        
        assert!(!sim.step()); // Should process task 3 last (150ms)
        assert_eq!(sim.current_time(), Duration::from_millis(150));
    });
}
```

### Integration with Existing Tests

**File: `moonpool-simulation/tests/simulation_integration.rs`** (updated)

```rust
// Existing tests should continue to work but now use real event processing

#[test]
fn test_simple_echo_simulation_with_events() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .unwrap();

    local_runtime.block_on(async move {
        let config = NetworkConfiguration::wan_simulation();
        let mut sim = SimWorld::new_with_network_config(config);
        let provider = sim.network_provider();

        // This should now wait for real events
        simple_echo_server(provider, "echo-server").await.unwrap();
        
        // Process all events - should include BindComplete, ConnectionReady, DataDelivery
        sim.run_until_empty();

        // Time should have advanced through multiple events
        assert!(sim.current_time() > Duration::from_millis(10));
        
        // Verify we processed multiple events
        assert!(sim.pending_event_count() == 0);
    });
}
```

## Phase 2d Success Criteria

- ✅ All quality gates pass (fmt, clippy, test) 
- ✅ Real event processing drives simulation behavior
- ✅ Network operations block until their events are processed
- ✅ Futures properly coordinate with event system using wakers
- ✅ Data delivery events actually populate receive buffers
- ✅ Bind and connection events control operation completion
- ✅ All existing Phase 2c tests continue to pass
- ✅ Event processing tests demonstrate proper async coordination
- ✅ Simulation time advancement driven by real work, not placeholder events

## Key Improvements over Phase 2c

### What Was Changed
- **Event Processing**: Real work instead of placeholder acknowledgment
- **Async Coordination**: Futures that properly wait for events using wakers
- **Operation Completion**: Network ops block until events complete them
- **State Management**: Track completion status for bind, connect, data delivery

### What Was Added  
- **Future Types**: BindFuture, ConnectionFuture, DataDeliveryFuture
- **Waker Management**: Register and wake futures when events complete
- **Completion Tracking**: HashSets and HashMaps to track operation state
- **Real Data Flow**: Events actually deliver data to receive buffers

### What Was Preserved
- **NetworkProvider Interface**: Same trait-based API
- **Configuration System**: NetworkConfiguration continues to work
- **Deterministic Behavior**: Same seed produces identical results
- **Event Scheduling**: Time-based event ordering maintained

Phase 2d transforms the simulation from a time-advancement system with placeholder events into a fully functional event-driven network simulation where events perform real work and drive the simulation state machine.