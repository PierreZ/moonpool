# Chinese Article Summary: FoundationDB Network Implementation

## Reference Files
- `fdbrpc/FlowTransport.actor.cpp` - Main transport layer implementation
- `fdbrpc/FlowTransport.h` - Transport layer interfaces and Peer class
- `flow/Net2.actor.cpp` - Network implementation with Connection class
- `fdbrpc/fdbrpc.h` - RPC interfaces and endpoint definitions
- `fdbrpc/NetworkSender.actor.h` - Network sending actor
- `fdbserver/ClusterController.actor.cpp` - Connection monitoring examples
- `flow/Knobs.cpp` - Network timeouts and parameters
- `fdbserver/workloads/Ping.actor.cpp` - Network testing workload
- Blog: "FoundationDB flow -- Part 1" (diabloneo.github.io)
- Blog: "FoundationDB flow -- Part 2" (diabloneo.github.io)
- GitHub issues: #2463, #1640 - FlowTransport discussions

## Flow Basics (from Part 1)

### Core Concepts
- **Single-threaded execution**: All Flow code runs in one thread
- **Task priority system**: Controls execution order deterministically
- **SAV (Single Assignment Variable)**: Core primitive for Promise/Future
- **Main loop**: `g_network->run()` processes ready tasks by priority

### Network Initialization
```cpp
g_network = newNet2(NetworkAddress(), false);
setupNetwork();
runNetwork();  // Calls g_network->run()
```

### Task Scheduling
- Tasks added to ready queue when futures complete
- Processed in priority order
- Higher priority tasks preempt lower ones
- Ensures deterministic execution

## Network Architecture Overview (from Part 2)

### 1. Connection Acceptance Flow
- `FlowTransport::bind()` starts listening
- `listen()` actor accepts new connections
- Each connection spawns `connectionIncoming()` actor
- `connectionIncoming()` creates three child actors:
  - `connectionKeeper`: Manages connection lifecycle
  - `connectionReader`: Reads incoming packets
  - `connectionWriter`: Sends outgoing messages

### 2. Message Flow Details

#### Receiving Messages
```cpp
// connectionReader continuously reads packets
while(true) {
    PacketData = wait(conn->read());
    scanPackets(data);  // Routes to endpoints
}
```

#### Sending Messages
```cpp
// Application code (synchronous feel)
FlowTransport::transport().sendUnreliable(msg, endpoint);

// Actually queues in peer->unsent
peer->unsent.push(message);

// connectionWriter drains queue (asynchronous)
while(!peer->unsent.empty()) {
    packet = peer->unsent.front();
    wait(conn->write(packet));
    peer->unsent.pop();
}
```

### 3. Endpoint System
- Each service registers endpoints with unique tokens (UIDs)
- `FlowTransport::addWellKnownEndpoint()` maps tokens to handlers
- `scanPackets()` uses token to route messages
- No centralized message bus - direct routing

### 4. Message Serialization
- `ReplyPromise` serialization is special
- Automatically creates `networkSender` actor
- Enables request/response pattern
- Uses traits system for automatic handling

### 5. Connection Lifecycle

#### Startup Sequence
1. `bind()` creates listener
2. `accept()` returns new connections
3. `connectionIncoming()` spawns handlers
4. Three actors manage each connection

#### Failure Handling
- `connectionMonitor` pings every second
- Timeout triggers reconnection
- Exponential backoff on failures
- Queued messages preserved

## Critical Design Insights for Implementation

### 1. Actor-per-Connection Pattern
```
Transport
  └── Peer
        ├── connectionKeeper (coordinates)
        ├── connectionReader (receives)
        ├── connectionWriter (sends)
        └── unsent queue (buffer)
```

### 2. Asynchronous Queueing
- **Key insight**: `send()` never blocks
- Messages queue in `peer->unsent`
- `connectionWriter` drains asynchronously
- Natural backpressure through queue growth

### 3. Message Delivery Guarantees
- **Reliable**: Tracked in separate queue, must deliver
- **Unreliable**: Can be dropped on failure
- Order preserved within each type
- No global ordering across types

### 4. Zero-Copy Design
- Arena allocators for messages
- References passed, not copies
- Efficient for large messages
- Memory lifetime managed by arenas

### 5. Protocol Versioning
- Version negotiated on connection
- Incompatible versions rejected
- Allows protocol evolution
- Maintains backward compatibility

## Implementation Recommendations

### Essential Patterns

1. **Decouple API from I/O**
   - Public `send()` just queues
   - Private actor does actual I/O
   - Hides network complexity

2. **Hierarchical Actor Management**
   ```cpp
   // Parent manages children
   Future<Void> keeper = connectionKeeper();
   Future<Void> reader = connectionReader();
   Future<Void> writer = connectionWriter();
   
   choose {
       when(wait(keeper)) { /* restart all */ }
       when(wait(reader)) { /* connection failed */ }
       when(wait(writer)) { /* connection failed */ }
   }
   ```

3. **Deterministic Testing**
   - All I/O through interfaces
   - Time through abstraction
   - Randomness controlled
   - Network delays simulated

### Configuration Parameters (from Knobs.cpp)
- `CONNECTION_MONITOR_TIMEOUT`: 2.0s (6.0s with BUGGIFY)
- `CONNECTION_MONITOR_IDLE_TIMEOUT`: 180.0s
- `CONNECTION_ID_TIMEOUT`: 600.0s
- `INITIAL_RECONNECTION_TIME`: 0.05s
- `MAX_RECONNECTION_TIME`: 0.5s
- `RECONNECTION_TIME_GROWTH_RATE`: 1.2
- `ACCEPT_BATCH_SIZE`: 20 (connections per batch)

### Testing Strategy
- Use simulation to control scheduling
- Inject failures at actor boundaries
- Verify queue depths and ordering
- Test with deterministic seeds
- Reproduce bugs exactly
