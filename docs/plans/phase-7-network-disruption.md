# Phase 7: Network Disruption for Bug Discovery

## Overview

Phase 7 implements probabilistic network disruption mechanisms to trigger edge cases and improve the effectiveness of our ping-pong tests for finding distributed systems bugs. This phase builds on the existing simulation framework to add realistic network chaos patterns.

## Motivation

Current ping-pong tests show predictable behavior with 100% success rates for some assertions and 0% for others:
- `peer_queue_grows` (0%) - Queue never grows beyond single message
- `peer_recovers_after_failures` (0%) - No failures occur to recover from  
- `peer_reuses_connection` (100%) - Always reuses connections
- `peer_sends_without_queue` (100%) - Never experiences queuing delays

These patterns indicate the test is too well-behaved and doesn't stress the peer system enough to reveal edge cases and potential bugs.

## Phase 7a: Network Clogging Implementation

**Goal**: Implement temporary I/O blocking to create queue buildup and trigger `peer_queue_grows`.

### Current State Analysis

Our simulation framework has:
- Configurable network latencies via `NetworkConfiguration`
- Connection-level event scheduling in `SimWorld`
- Stream operations that can return `Poll::Pending`
- No mechanism to temporarily block I/O operations

### Implementation Steps

1. **Add clogging configuration to NetworkConfiguration**
   ```rust
   // In src/network/config.rs
   #[derive(Debug, Clone)]
   pub struct NetworkConfiguration {
       pub latency: LatencyConfiguration,
       pub clogging: CloggingConfiguration, // NEW
   }

   #[derive(Debug, Clone)]
   pub struct CloggingConfiguration {
       /// Probability per I/O operation that clogging occurs (0.0 - 1.0)
       pub probability: f64,
       
       /// How long clogging lasts
       pub duration: LatencyRange,
       
       /// Which operations can be clogged
       pub operations: ClogOperations,
   }

   #[derive(Debug, Clone)]
   pub struct ClogOperations {
       pub connect: bool,
       pub read: bool, 
       pub write: bool,
   }
   ```

2. **Add clogging state to SimWorld**
   ```rust
   // In src/sim.rs
   #[derive(Debug)]
   struct ClogState {
       expires_at: SystemTime,
       clogged_operations: ClogOperations,
   }

   struct SimInner {
       // ... existing fields ...
       connection_clogs: HashMap<ConnectionId, ClogState>,
       clog_wakers: HashMap<ConnectionId, Vec<Waker>>,
   }
   ```

3. **Implement clogging logic in SimWorld**
   ```rust
   impl SimWorld {
       fn should_clog_operation(&self, connection_id: ConnectionId, op_type: OperationType) -> bool {
           // Probability check + already clogged check
       }
       
       fn clog_connection(&mut self, connection_id: ConnectionId, op_type: OperationType) {
           // Set clog expiry time and operations
       }
       
       fn is_connection_clogged(&self, connection_id: ConnectionId, op_type: OperationType) -> bool {
           // Check if connection is currently clogged for this operation
       }
   }
   ```

4. **Integrate with stream operations**
   ```rust
   // In src/network/sim/stream.rs  
   impl TcpStreamTrait for SimTcpStream {
       fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
           // Check for clogging before normal write logic
           if sim.is_connection_clogged(self.connection_id, OperationType::Write) {
               sim.register_clog_waker(self.connection_id, cx.waker().clone());
               return Poll::Pending; // Block until clog clears
           }
           // ... existing logic ...
       }
   }
   ```

5. **Add clog cleanup to tick processing**
   ```rust
   impl SimWorld {
       pub fn tick(&mut self) -> bool {
           // ... existing tick logic ...
           self.clear_expired_clogs();
           // ... rest of tick ...
       }
   }
   ```

### Test Configurations

```rust
impl NetworkConfiguration {
    pub fn clogged_simulation() -> Self {
        let mut config = Self::wan_simulation();
        config.clogging = CloggingConfiguration {
            probability: 0.15, // 15% chance per I/O operation
            duration: LatencyRange::new(
                Duration::from_millis(50),
                Duration::from_millis(300),
            ),
            operations: ClogOperations {
                connect: false, // Don't clog initial connections
                read: false,    // Don't clog reads (would break protocol)
                write: true,    // Clog writes (causes queue buildup)
            },
        };
        config
    }
}
```

## Phase 7b: Connection Cutting Implementation

**Goal**: Implement probabilistic connection termination to trigger `peer_recovers_after_failures`.

### Current State Analysis

Our simulation framework can:
- Create connection pairs via `SimWorld::create_connection_pair`
- Track connections in `inner.connections` HashMap
- Handle connection failures during initial establishment
- **Missing**: Ability to terminate existing established connections

### Implementation Steps

1. **Add connection cutting configuration**
   ```rust
   #[derive(Debug, Clone)]
   pub struct NetworkConfiguration {
       pub latency: LatencyConfiguration,
       pub clogging: CloggingConfiguration,
       pub cutting: CuttingConfiguration, // NEW
   }

   #[derive(Debug, Clone)]
   pub struct CuttingConfiguration {
       /// Probability per connection per tick (0.0 - 1.0)
       pub probability: f64,
       
       /// How long before reconnection is possible
       pub reconnect_delay: LatencyRange,
       
       /// Maximum cuts per connection
       pub max_cuts_per_connection: Option<u32>,
   }
   ```

2. **Add cutting state tracking**
   ```rust
   #[derive(Debug, Clone)]
   struct CutState {
       /// Original connection data (for restoration)
       connection_data: ConnectionState,
       
       /// When connection can be restored
       reconnect_at: SystemTime,
       
       /// Cut count for this connection
       cut_count: u32,
   }

   struct SimInner {
       // ... existing fields ...
       cut_connections: HashMap<ConnectionId, CutState>,
       cut_wakers: HashMap<ConnectionId, Vec<Waker>>,
   }
   ```

3. **Implement connection cutting logic**
   ```rust
   impl SimWorld {
       pub fn cut_connection(&mut self, connection_id: ConnectionId) {
           // Remove from active connections, store for restoration
       }
       
       pub fn restore_cut_connections(&mut self) {
           // Check for connections ready to be restored
       }
       
       fn randomly_cut_connections(&mut self) {
           // Probabilistically cut active connections each tick
       }
   }
   ```

4. **Handle cut connections in stream operations**
   ```rust
   impl TcpStreamTrait for SimTcpStream {
       fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
           if sim.is_connection_cut(self.connection_id) {
               sim.register_cut_waker(self.connection_id, cx.waker().clone());
               return Poll::Ready(Err(io::Error::new(
                   io::ErrorKind::ConnectionReset, 
                   "Connection was cut"
               )));
           }
           // ... normal logic ...
       }
   }
   ```

## Phase 7c: Combined Test Configuration

**Goal**: Create test configurations that combine both clogging and cutting for maximum chaos.

### Implementation Steps

1. **Create aggressive disruption configuration**
   ```rust
   impl NetworkConfiguration {
       pub fn chaos_simulation() -> Self {
           let mut config = Self::wan_simulation();
           
           config.clogging = CloggingConfiguration {
               probability: 0.20, // 20% chance per write operation
               duration: LatencyRange::new(
                   Duration::from_millis(100),
                   Duration::from_millis(500),
               ),
               operations: ClogOperations {
                   connect: false,
                   read: false, 
                   write: true, // Only clog writes
               },
           };
           
           config.cutting = CuttingConfiguration {
               probability: 0.03, // 3% chance per connection per tick
               reconnect_delay: LatencyRange::new(
                   Duration::from_millis(200),
                   Duration::from_millis(800),
               ),
               max_cuts_per_connection: Some(2),
           };
           
           config
       }
   }
   ```

2. **Update ping-pong test to use chaos configuration**
   ```rust
   #[test]
   fn test_ping_pong_with_network_chaos() {
       let report = SimulationBuilder::new()
           .set_network_config(NetworkConfiguration::chaos_simulation())
           .register_workload("ping_pong_server", ping_pong_server)
           .register_workload("ping_pong_client", ping_pong_client)
           .set_iteration_control(IterationControl::FixedCount(51))
           .run()
           .await;
           
       // Test should now have varied assertion frequencies
   }
   ```

## Expected Behavior

With Phase 7 implemented, the ping-pong test should experience realistic network chaos:

### Clogging Effects (Phase 7a)
- 15-20% of write operations temporarily block 
- `peer.send()` calls return `Poll::Pending` during clogs
- Messages queue up in peer's internal send buffer
- When clogs clear (50-500ms later), operations complete rapidly
- **Result**: Triggers `peer_queue_grows` assertion

### Connection Cutting Effects (Phase 7b)  
- 3% chance per tick that connections get terminated
- All pending I/O operations fail with `ConnectionReset` errors
- Peer must reconnect and retry queued messages
- **Result**: Triggers `peer_recovers_after_failures` assertion

### Combined Chaos Effects (Phase 7c)
- Variable connection reuse patterns (sometimes fresh, sometimes reused)
- Mixed direct sends vs queued sends depending on network state
- Queue capacity stress testing under realistic load
- **Result**: All sometimes assertions show varied frequencies (10-90%)

## Implementation Order

1. **Phase 7a**: Network clogging (I/O blocking)
2. **Phase 7b**: Connection cutting (connection termination) 
3. **Phase 7c**: Combined test configuration and validation

## Success Criteria

- `peer_queue_grows` assertion hits 10-90% success rate
- `peer_recovers_after_failures` assertion hits 10-90% success rate
- All other sometimes assertions show more varied frequencies (not 0% or 100%)
- Ping-pong test remains stable with network disruptions
- No deadlocks or infinite loops during disruption events

## Risk Mitigation

- **Deadlock Prevention**: Careful waker management to avoid blocking forever
- **Memory Leaks**: Proper cleanup of expired clogs and cut connections
- **Race Conditions**: Consistent borrowing patterns for SimWorld state
- **Test Stability**: Bounded disruption parameters to avoid excessive chaos

This phase transforms the ping-pong test from a predictable happy-path scenario into a realistic chaos testing framework that can discover distributed systems bugs.