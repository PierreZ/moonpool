# Moonpool Simulation Framework Specification

## Overview

The Moonpool Simulation Framework provides a deterministic, reproducible simulation environment for testing distributed systems in Rust. The primary goal is to allow users to seamlessly switch between simulated and real Tokio implementations without code changes, enabling the same application logic to run in production with real networking and in tests with controlled simulation.

**Core Philosophy: Find Bugs Before Production**
The framework is designed to torture distributed systems with the worst possible infrastructure conditions. By simulating catastrophic network failures, extreme delays, and chaotic fault combinations, we expose bugs that would otherwise only surface in production disasters. The goal is to make tests more hostile than production environments.

The framework offers a comprehensive I/O simulation layer that can replace real network and time operations with simulated equivalents, enabling rigorous testing of distributed system behaviors under various failure conditions.

## Design Goals

1. **Keep It Simple**: The framework must remain simple to use, understand, and maintain - complexity is the enemy of correctness
2. **Trait-Based Abstraction**: Applications depend on traits (like `NetworkProvider`), not concrete implementations
3. **Seamless Runtime Switching**: Switch between simulated and real implementations by changing the provider
4. **Torture Test Infrastructure**: Simulate the worst possible network conditions to find bugs before production
5. **Deterministic Execution**: All runs with the same seed produce identical behavior
6. **Tokio Compatibility**: Simulated types implement the same traits as Tokio's networking primitives
7. **Triple Interface**: Library, runtime replacement, and testing framework
8. **Comprehensive I/O**: Network and time simulation with extensibility for file I/O
9. **Fault Injection**: Controllable network failures, delays, and partitions

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    Application Code                         │
├─────────────────────────────────────────────────────────────┤
│              Tokio-Compatible Traits                       │
│  (TcpStream, TcpListener, Instant, Duration, etc.)        │
├─────────────────────────────────────────────────────────────┤
│                Simulation Runtime                          │
│  ┌─────────────────┐  ┌─────────────────┐                 │
│  │   Time Engine   │  │ Network Engine  │                 │
│  │                 │  │                 │                 │
│  │ • Logical Time  │  │ • Connections   │                 │
│  │ • Event Queue   │  │ • Packet Sim    │                 │
│  │ • Scheduling    │  │ • Fault Model   │                 │
│  └─────────────────┘  └─────────────────┘                 │
│                         │                                   │
│  ┌─────────────────────────────────────────────────────────┤
│  │           Deterministic Random Engine               │
│  │                  (Seed-based)                       │
│  └─────────────────────────────────────────────────────────┘
└─────────────────────────────────────────────────────────────┘
```

## Core Components

### 1. Simulation World (`SimWorld`)

The central coordinator that maintains the simulation state. It acts as the global clock and event dispatcher, ensuring all simulated operations happen in deterministic order.

**Responsibilities:**
- Advance logical time by processing events
- Coordinate between time and network engines (future phases)
- Maintain deterministic execution order
- Provide global simulation state

### 2. Time Engine

Provides deterministic time simulation using logical time that advances only when events are processed. This ensures reproducible timing behavior across test runs.

**Key Features:**
- Logical time only (no wall-clock dependency)
- Event-driven time advancement
- Compatible with `std::time` APIs
- Deterministic sleep/timeout behavior

### 3. Network Engine

Simulates network connections and failures between nodes. Messages are queued and delivered based on configured network conditions, allowing realistic testing of distributed system behaviors under network stress.

**Architecture:**
- **Connection Registry**: Maintains all active simulated connections with unique identifiers
- **Message Queuing**: Per-connection message queues with delivery scheduling based on network conditions
- **Fault Injection Engine**: Applies packet loss, delays, and partitions based on configuration
- **Address Resolution**: Maps string addresses to internal node identifiers for routing

**Connection Lifecycle:**
1. Connection establishment creates registry entry and assigns unique ID
2. Messages are queued with delivery times calculated from network conditions
3. Fault injection may drop, delay, or duplicate messages during queuing
4. Event system schedules message delivery at calculated times
5. Connection cleanup removes registry entries and flushes queues

**Capabilities:**
- Point-to-point connection simulation with realistic establishment/teardown
- Configurable packet loss, delays, and bandwidth limitations
- Network partition simulation (isolating groups of nodes)
- Message reordering and duplication based on network conditions
- Connection failure injection (half-open connections, sudden disconnects)

### 4. Deterministic Random Engine

Provides seed-based randomness for reproducible behavior across simulation runs. The same seed will always produce the same sequence of network failures, delays, and other random events.

**Multi-Seed Replay Testing:**
The framework supports running the same test with multiple different seeds to comprehensively test edge cases. This is crucial for distributed systems where rare random sequences can expose subtle bugs. Tests automatically iterate through a collection of seeds, with each iteration using a fresh Tokio runtime configured with the specific seed for that run.

## Ownership Model

One of the key challenges in simulation frameworks is managing complex ownership patterns while maintaining performance and avoiding borrow checker conflicts. The Moonpool simulation framework addresses this through a specific ownership architecture.

### Centralized State with Handle Pattern

**Core Principle: Single Owner, Handle-Based Access**

The simulation uses a centralized ownership model where:
- A single `Sim` "world" owns all mutable state (time, RNG, registries)
- Public objects are lightweight **handles** that carry `{ WeakSim, Id }`
- Handles never hold direct borrows to avoid nested borrowing issues

### Handle Lifecycle

**Short-Lived Borrows:**
Handles briefly upgrade their weak reference when performing operations:
1. Upgrade `WeakSim` → borrow registry
2. Schedule event or perform operation
3. Release borrow and return

This pattern prevents long-lived borrows that would block other operations.

### Event-Driven State Management

**Avoiding Cross-Module Borrowing:**
- Each event touches only one registry at a time
- Cross-module effects are handled by scheduling additional events
- No direct cross-registry calls that would require nested borrows

### Future State Storage

**Minimal Future State:**
Async futures store only essential data:
- `{ WeakSim, Id, small state }`
- No borrowed references that would complicate lifetime management
- State reconstruction on each poll from the central registries

### Benefits of This Model

1. **Predictable Ownership**: Clear ownership hierarchy prevents borrowing conflicts
2. **Scalable**: Can handle complex simulations without lifetime entanglements
3. **Deterministic**: Centralized state ensures consistent event ordering
4. **Testable**: Clean separation makes the simulation framework itself testable

### Ownership Challenges and Solutions

**Challenge**: Traditional Rust async patterns often lead to complex lifetime dependencies in simulation contexts.

**Solution**: The handle pattern with weak references allows:
- Simulation components to be created and destroyed independently
- Clear ownership boundaries between simulation infrastructure and user code
- Efficient state access without long-lived borrows

## Event System

### Event Types

The simulation operates on discrete events scheduled for future execution:

- **Wake Events**: Resume sleeping tasks at specific times
- **Network Messages**: Deliver messages between nodes with realistic delays
- **Connection Events**: Handle connection establishment, closure, and failures
- **Network Faults**: Inject network partitions, packet loss, and other failures

### Event Processing

Events are processed in strict chronological order:

1. Pop earliest event from priority queue
2. Advance logical time to event timestamp
3. Execute event handler
4. Process any immediately scheduled events
5. Repeat until queue is empty or termination condition

## Tokio Compatibility Layer

The framework provides trait-based abstractions that allow compile-time or runtime switching between simulated and real implementations.

### Network Provider Pattern

Rather than calling networking functions directly, users work with a `NetworkProvider` trait that abstracts connection and listener creation. This allows the same application code to work with both real Tokio networking and simulated networking by simply changing the provider implementation.

**Core Pattern:**
- Applications depend on `NetworkProvider` trait, not concrete types
- Real implementation uses Tokio's networking primitives
- Simulated implementation uses the simulation framework
- Same application logic works with both providers

### Network Abstractions

The simulated network types implement the exact same traits as Tokio's networking primitives (`AsyncRead`, `AsyncWrite`, etc.), ensuring compatibility at the I/O level. Connection and Listener types produced by the NetworkProvider work identically whether they're real or simulated.

### Time Abstractions

Simulated time operations provide the same interface as `std::time` and Tokio's time utilities, but work with logical time instead of wall-clock time. Sleep operations schedule wake events in the simulation queue rather than blocking on actual time passage, enabling deterministic and fast test execution.

## Usage Patterns

### 1. Library Integration

Direct integration with the simulation framework for fine-grained control over network conditions and fault injection. Users create a simulation world and explicitly manage network topology and failure scenarios.

### 2. Runtime Replacement

Applications use dependency injection with the `NetworkProvider` trait. The same application logic works with different provider implementations - real Tokio networking for production and simulated networking for tests. This eliminates the need for conditional compilation or code duplication.

### 3. Testing Framework

High-level testing harness with built-in patterns for distributed systems testing. Provides macros and utilities for common scenarios like cluster setup, network partitions, and time-bounded assertions.

**Multi-Seed Replay Pattern:**
The testing framework automatically runs each test with multiple seeds to ensure comprehensive coverage of random behaviors. Each test iteration uses a fresh Tokio runtime with deterministic seeding, allowing tests to explore different random sequences while maintaining reproducibility. This pattern is essential for catching race conditions and edge cases that only manifest with specific random event orderings.

**Torture Testing Philosophy:**
The framework enables aggressive testing of all code paths with infrastructure conditions worse than production. Every network operation should be tested under extreme delays, every connection should face random failures, and every timeout should be stress-tested. The goal is to make distributed systems antifragile by exposing them to chaos during development.

**Hostile Infrastructure Simulation:**
- Network delays that are 10x worse than production
- Packet loss rates that would cripple real systems
- Connection failures at the worst possible moments
- Combinations of faults that create perfect storms
- Time dilation to expose race conditions

**Simplicity First:**
The framework prioritizes simplicity over feature completeness. Complex behaviors should be composed from simple primitives rather than built as monolithic features. If a feature makes the framework harder to understand or use, it should be reconsidered or simplified.

## Configuration

### Simulation Configuration

Global configuration controlling simulation behavior including deterministic seed, network fault parameters, execution bounds, and debugging options.

**Seed Replay Configuration:**
- Base seed collection for multi-iteration testing
- Number of replay iterations per test
- Seed generation strategy (fixed set, random generation, or hybrid)
- Runtime configuration for each seed iteration (paused time, RNG seeding)

### Network Fault Configuration

Configurable network behavior for realistic distributed system testing scenarios:

**Packet-Level Faults:**
- Packet loss rates (per connection or global)
- Message duplication probability
- Out-of-order delivery simulation

**Timing Faults:**
- Delay patterns: constant, uniform distribution, exponential distribution
- Jitter simulation for variable network conditions
- Connection establishment delays

**Partition Faults:**
- Network partition scenarios (split-brain, minority isolation)
- Asymmetric partitions (A can reach B, but B cannot reach A)
- Rolling partitions that affect different nodes over time

**Connection Faults:**
- Connection establishment failures
- Mid-stream connection drops
- Half-open connection simulation
- Backpressure and flow control issues

**Aggressive Fault Injection:**
All fault types are designed to be easily enabled in tests with extreme parameters that stress-test error handling. The simulation framework makes it trivial to create infrastructure conditions that are worse than any real production environment.

**Torture Test Defaults:**
Default configurations should be pessimistic, creating hostile conditions that expose weaknesses:
- High packet loss rates (10-50% instead of realistic 0.1%)
- Extreme delays (seconds instead of milliseconds) 
- Frequent connection failures
- Aggressive partition scenarios

**Simple Configuration:**
Fault injection should be configurable through simple, understandable parameters. Complex network topologies or failure patterns should be built by composing simple fault types rather than requiring complex configuration schemas.

## Implementation Phases

### Phase 1: Core Infrastructure
- [ ] Logical time engine
- [ ] Event queue and processing
- [ ] Deterministic RNG
- [ ] Basic simulation harness

### Phase 2: Network Simulation  
- [ ] Point-to-point connections
- [ ] Message delivery simulation
- [ ] Basic fault injection (delays, drops)

### Phase 3: Tokio Integration
- [ ] AsyncRead/AsyncWrite implementations
- [ ] TcpStream/TcpListener simulation
- [ ] Time APIs (sleep, timeout, Instant)

### Phase 4: Testing Framework
- [ ] Simple, intuitive test harness and macros
- [ ] Multi-seed replay testing infrastructure
- [ ] Built-in fault injection helpers for aggressive testing
- [ ] Default test configurations with hostile infrastructure conditions
- [ ] Torture test presets (catastrophic failures, extreme delays)
- [ ] Simple assertion utilities
- [ ] Deterministic runtime creation per seed iteration
- [ ] Easy chaos engineering for distributed systems
- [ ] Ensure all APIs remain simple and discoverable

### Phase 5: Advanced Features
- [ ] Network partitions
- [ ] Message reordering
- [ ] Connection failure simulation
- [ ] Performance optimization

## Open Questions

1. **Task Scheduling**: How should we handle async task scheduling within the simulation, particularly across multiple seed replay iterations?
2. **Seed Management**: What's the optimal strategy for seed collection and replay iteration management?
3. **Memory Model**: Should we simulate memory operations or assume perfect local memory?
4. **External Integration**: How should the simulator interact with real external services?
5. **Performance**: What are acceptable performance trade-offs for simulation accuracy, especially with multi-seed testing overhead?
6. **Debugging**: What debugging and introspection tools should be built-in for analyzing failures across multiple seed iterations?
7. **Simplicity vs Features**: How do we maintain simplicity while providing comprehensive testing capabilities?
8. **Torture vs Realism**: How extreme should default fault conditions be while still being useful for development?

## References

- FoundationDB Simulation Framework
- TigerBeetle VSR Network Simulator
- Tokio Runtime Architecture
- Jepsen Testing Methodology