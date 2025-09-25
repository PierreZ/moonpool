# Transport Layer Specification

## Overview

The Moonpool Transport Layer implements a Sans I/O network transport that provides request-response semantics on top of raw TCP connections. This specification describes the architecture, design principles, and implementation of Phase 11 of the Moonpool framework.

## Design Principles

1. **Sans I/O Pattern**: Protocol logic separated from actual I/O operations for comprehensive testability
2. **Envelope-based Messaging**: All communication through typed envelopes with correlation IDs
3. **Self-Driving Futures**: Transport operations internally manage necessary state advancement
4. **Type Safety**: Compile-time guarantees for message types and correlation
5. **Chaos Testing**: Built-in support for deterministic fault injection
6. **Provider Pattern**: Abstract dependencies for seamless simulation/real network switching

## Architecture

### Layered Design

```
┌─────────────────────────────────────────────────────────────┐
│                    Application Code                         │
├─────────────────────────────────────────────────────────────┤
│                Transport Layer API                         │
│  ┌─────────────────┐  ┌─────────────────┐                 │
│  │ ClientTransport │  │ ServerTransport │                 │
│  │                 │  │                 │                 │
│  │ • request()     │  │ • try_next_msg()│                 │
│  │ • Self-driving  │  │ • Event-driven  │                 │
│  │ • Correlation   │  │ • Multi-conn    │                 │
│  └─────────────────┘  └─────────────────┘                 │
├─────────────────────────────────────────────────────────────┤
│                    Driver Layer                            │
│  ┌─────────────────────────────────────────────────────────┤
│  │ • Protocol-to-Peer Bridge  • Buffer Management     │
│  │ • I/O Operation Handling   • Future Coordination   │
│  └─────────────────────────────────────────────────────────┘
├─────────────────────────────────────────────────────────────┤
│                Sans I/O Protocol                           │
│  ┌─────────────────────────────────────────────────────────┤
│  │ • Pure State Machine      • Envelope Processing    │
│  │ • Correlation ID Logic    • Protocol States        │
│  │ • No I/O Operations       • Testable in Isolation  │
│  └─────────────────────────────────────────────────────────┘
├─────────────────────────────────────────────────────────────┤
│                  Envelope System                           │
│  ┌─────────────────────────────────────────────────────────┤
│  │ • Type-Safe Messages      • Binary Serialization   │
│  │ • Correlation IDs         • Length-Prefixed Wire   │
│  │ • Request/Response Pairs  • Efficient Encoding     │
│  └─────────────────────────────────────────────────────────┘
├─────────────────────────────────────────────────────────────┤
│                    Peer Layer                              │
│  ┌─────────────────────────────────────────────────────────┤
│  │ • TCP Connection Management  • Simulation/Real      │
│  │ • AsyncRead/AsyncWrite       • Provider Pattern     │
│  └─────────────────────────────────────────────────────────┘
└─────────────────────────────────────────────────────────────┘
```

## Core Components

### 1. Envelope System

Type-safe message containers that provide the foundation for all transport communication.

**Key Features:**
- Correlation ID tracking for request-response semantics
- Binary serialization with length-prefixed wire format
- Type safety through generics and marker traits
- Efficient encoding/decoding for network transmission

**Architecture:**
- `Envelope<T>`: Generic container for typed messages
- `EnvelopeSerializer`: Trait for binary wire format conversion
- `RequestResponseEnvelopeFactory`: Creates correlated message pairs
- `InsufficientData` error handling for partial message reception

### 2. Sans I/O Protocol

Pure state machine that handles all protocol logic without any I/O operations.

**Benefits:**
- Complete testability in isolation from networking
- Deterministic behavior independent of I/O timing
- Clear separation of concerns between logic and I/O
- Comprehensive unit testing of protocol states

**Responsibilities:**
- Message correlation tracking
- Request/response state management
- Protocol state transitions
- Buffer management coordination
- Error condition handling

### 3. Driver Layer

Bridges the Sans I/O protocol with actual networking operations.

**Architecture:**
- Wraps Sans I/O protocol with I/O capabilities
- Manages read/write buffers for network operations
- Coordinates between protocol state and peer connections
- Handles async future orchestration
- Provides self-driving operation semantics

**Key Methods:**
- Buffer management for partial reads/writes
- Event-driven I/O operation scheduling
- Protocol state synchronization
- Error propagation and handling

### 4. Transport APIs

High-level async APIs that hide transport complexity behind clean interfaces.

#### ClientTransport

**Primary Method:** `request<TRequest, TResponse>(message: TRequest) -> Result<TResponse, Error>`

**Features:**
- Self-driving request-response operations
- Automatic correlation ID management
- Built-in timeout and retry logic
- Transparent reconnection on failures
- Type-safe message handling with turbofish pattern

**Usage Pattern:**
- Create envelope with request message
- Send request and wait for correlated response
- Handle network failures with automatic retry
- Return typed response or detailed error

#### ServerTransport

**Primary Method:** `try_next_message() -> Result<Option<(TRequest, ResponseSender)>, Error>`

**Features:**
- Event-driven message reception
- Multi-connection support with automatic multiplexing
- Clean async/await API without manual driving
- Type-safe envelope handling
- Connection lifecycle management

**Usage Pattern:**
- Poll for incoming messages across all connections
- Receive typed request with response sender
- Process request and send typed response
- Handle connection establishment/teardown automatically

## Implementation Phases

The transport layer was implemented in 7 incremental phases:

### ✅ Phase 11.0: Core Envelope Traits (161 lines)
- Foundation envelope traits and marker types
- Test infrastructure setup
- Core abstractions for entire transport layer

### ✅ Phase 11.1: Binary Serialization (120 lines)
- Length-prefixed wire format for efficient network transmission
- Serialization/deserialization for envelope system
- Binary encoding with proper error handling

### ✅ Phase 11.2: Sans I/O Protocol (336 lines)
- Pure state machine for protocol logic
- No I/O operations, completely testable
- Comprehensive protocol state management

### ✅ Phase 11.3: Protocol Driver (175 lines)
- Bridges Sans I/O protocol with actual networking
- Buffer management and I/O operation coordination
- Self-driving async future implementation

### ✅ Phase 11.4: Client Transport (338 lines)
- High-level client API with type-safe operations
- Self-driving request() method with correlation
- Automatic reconnection and error handling

### ✅ Phase 11.5: Server Transport (283 lines)
- Multi-connection server with event-driven reception
- Connection management and request handling
- End-to-end ping-pong test validation

### ✅ Phase 11.6: Chaos Testing Integration (131 lines)
- Deterministic fault injection for transport operations
- Connection failure simulation and recovery testing
- Network chaos scenarios with buggify integration

### ✅ Phase 11.7: Developer Experience Improvements
- FoundationDB-inspired API refinements
- Simplified server actor patterns (70% code reduction)
- Enhanced error handling and clean shutdown semantics

## Key Design Decisions

### Sans I/O Pattern Choice

**Benefits:**
- Complete protocol testability without networking
- Deterministic behavior in simulations
- Clear separation between logic and I/O timing
- Easier debugging and fault isolation

**Trade-offs:**
- Additional complexity in driver layer
- More components to coordinate
- Learning curve for developers unfamiliar with pattern

### Self-Driving Futures

**Rationale:**
- Eliminates manual transport.tick() calls from user code
- FoundationDB-proven pattern for distributed systems
- Reduces cognitive load on application developers
- Enables clean async/await patterns throughout

**Implementation:**
- Transport operations internally manage state advancement
- Futures coordinate with underlying protocol drivers
- Automatic I/O scheduling without explicit user coordination

### Correlation ID System

**Design:**
- Unique identifier for each request-response pair
- Automatic generation and tracking by transport layer
- Type-safe matching of requests with responses
- Built-in timeout and retry correlation

**Benefits:**
- Reliable message ordering in chaotic network conditions
- Multiple outstanding requests supported naturally
- Clear error handling for lost or delayed responses

## Integration with Simulation Framework

### Provider Pattern

The transport layer integrates with the simulation framework through the provider pattern:
- Same transport code works with simulated and real networking
- NetworkProvider abstraction enables seamless switching
- TimeProvider integration for deterministic timeouts
- TaskProvider coordination for async operation scheduling

### Chaos Testing

Built-in support for deterministic fault injection:
- Connection failures at strategic points
- Message delays and loss simulation
- Envelope corruption and partial reads
- Timeout scenarios with controlled timing

### Sometimes Assertions

Integration with the simulation testing philosophy:
- Strategic assertion placement for chaos coverage
- Statistical validation of error handling paths
- Comprehensive edge case exploration
- Deterministic reproduction of failures

## Testing Strategy

### Unit Testing
- Sans I/O protocol tested in complete isolation
- Envelope system validated without networking
- Driver layer tested with mock I/O operations
- State machine transitions comprehensively covered

### Integration Testing
- End-to-end ping-pong scenarios
- Multi-client/multi-server validation
- Connection multiplexing verification
- Error propagation and recovery testing

### Chaos Testing
- Deterministic fault injection across all components
- Network failure scenarios with automatic recovery
- Buffer management under extreme conditions
- Correlation tracking during message loss

## Performance Characteristics

### Memory Efficiency
- Minimal per-connection overhead
- Efficient buffer management in driver layer
- Zero-copy serialization where possible
- Connection pooling and reuse

### CPU Efficiency
- Event-driven architecture minimizes polling
- Efficient state machine transitions
- Optimized binary serialization format
- Lazy connection establishment

### Network Efficiency
- Length-prefixed messages minimize parsing overhead
- Correlation IDs enable pipeline parallelism
- Automatic connection reuse and management
- Optimal retry and timeout strategies

## Future Enhancements

### Phase 11.8: Multi-Connection Server Transport (Planned)
- Event-driven architecture for handling multiple concurrent connections
- Single queue design with connection source routing
- Rc/RefCell optimization for single-threaded runtime performance
- One actor per connection for clean Rust ownership patterns

### Additional Considerations
- Connection pooling and load balancing
- Advanced retry strategies and circuit breakers
- Metrics collection and observability integration
- Performance optimizations based on production usage

## References

- FoundationDB FlowTransport architecture
- Sans I/O pattern documentation
- Tokio async networking patterns
- TigerBeetle packet simulator design