# Moonpool Simulation Framework Specification

## Overview

The Moonpool Simulation Framework provides a deterministic, reproducible simulation environment for testing distributed systems in Rust. The primary goal is to allow users to seamlessly switch between simulated and real Tokio implementations without code changes, enabling the same application logic to run in production with real networking and in tests with controlled simulation.

**Core Philosophy: Find Bugs Before Production**
The framework is designed to torture distributed systems with the worst possible infrastructure conditions. By simulating catastrophic network failures, extreme delays, and chaotic fault combinations, we expose bugs that would otherwise only surface in production disasters. The goal is to make tests more hostile than production environments.

The framework offers a comprehensive I/O simulation layer that can replace real network and time operations with simulated equivalents, enabling rigorous testing of distributed system behaviors under various failure conditions.

## Specification Organization

This document provides an architectural overview of the entire Moonpool framework. For detailed specifications of individual components, see:

- **[Simulation Core Framework](simulation-core-spec.md)** - Core simulation infrastructure, event system, and provider pattern
- **[Transport Layer](transport-layer-spec.md)** - Sans I/O transport architecture with request-response semantics  
- **[Peer Networking](peer-networking-spec.md)** - Low-level TCP connection management and provider abstractions
- **[Testing Framework](testing-framework-spec.md)** - Chaos testing, assertions, and multi-seed testing infrastructure

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
│                Transport Layer (Sans I/O)                  │
│  ┌─────────────────┐  ┌─────────────────┐                 │
│  │ ClientTransport │  │ ServerTransport │                 │
│  │                 │  │                 │                 │
│  │ • request()     │  │ • try_next_msg()│                 │
│  │ • Self-driving  │  │ • Event-driven  │                 │
│  │ • Correlation   │  │ • Multi-conn    │                 │
│  └─────────────────┘  └─────────────────┘                 │
│                         │                                   │
│  ┌─────────────────────────────────────────────────────────┤
│  │           Driver + Protocol (Sans I/O)             │
│  │          • State Machine  • Buffer Management      │
│  │          • Envelope I/O   • Correlation IDs        │
│  └─────────────────────────────────────────────────────────┘
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

Simulates network connections and failures between nodes. Works in conjunction with the transport layer to provide realistic testing of distributed system behaviors under network stress. The transport layer handles envelope-based messaging while the network engine provides the underlying connection simulation.

**Architecture:**
- **Connection Registry**: Maintains all active simulated connections with unique identifiers
- **Transport Integration**: Works with envelope-based messaging system for type-safe communication
- **Fault Injection Engine**: Applies packet loss, delays, and partitions based on configuration
- **Address Resolution**: Maps string addresses to internal node identifiers for routing

**Connection Lifecycle:**
1. Connection establishment creates registry entry and assigns unique ID
2. Envelope messages are queued with delivery times calculated from network conditions
3. Fault injection may drop, delay, or duplicate envelopes during queuing
4. Event system schedules envelope delivery at calculated times
5. Connection cleanup removes registry entries and flushes queues
6. Transport layer handles correlation ID tracking for request-response semantics

**Enhanced Capabilities with Transport Integration:**
- Point-to-point connection simulation with realistic establishment/teardown
- Envelope-based message passing with automatic serialization/deserialization
- Built-in request-response correlation and timeout handling
- Configurable packet loss, delays, and bandwidth limitations
- Network partition simulation (isolating groups of nodes)
- Connection failure injection with automatic reconnection logic
- Multi-connection server support with proper connection multiplexing

### 4. Deterministic Random Engine

Provides seed-based randomness for reproducible behavior across simulation runs. The same seed will always produce the same sequence of network failures, delays, and other random events.

**Multi-Seed Replay Testing:**
The framework supports running the same test with multiple different seeds to comprehensively test edge cases. This is crucial for distributed systems where rare random sequences can expose subtle bugs. Tests automatically iterate through a collection of seeds, with each iteration using a fresh Tokio runtime configured with the specific seed for that run.

### 5. Transport Layer

The transport layer provides request-response semantics on top of raw TCP connections using the Sans I/O pattern. This architecture separates protocol logic from I/O operations, enabling comprehensive testing and deterministic behavior.

**Core Architecture:**
- **Envelope System**: Type-safe message containers with correlation IDs for reliable request-response tracking
- **Sans I/O Protocol**: Pure state machine handling protocol logic without any I/O operations
- **Driver Layer**: Bridges the Sans I/O protocol with actual networking, managing buffers and I/O operations
- **Client/Server Transport**: High-level async APIs that hide transport complexity behind clean interfaces

**Key Features:**
- **Self-Driving Futures**: Transport operations internally manage necessary ticking and state advancement
- **Automatic Correlation**: Request-response matching with unique correlation IDs
- **Connection Management**: Automatic reconnection on failures, multi-connection server support
- **Chaos Integration**: Built-in support for deterministic fault injection and testing
- **Type Safety**: Envelope system provides compile-time guarantees for message types

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

### Transport Abstractions

The transport layer provides high-level abstractions that work identically in both simulation and real networking environments:

**ClientTransport Pattern:**
- `request()` method for self-driving request-response operations
- Automatic reconnection and retry logic
- Type-safe envelope system with turbofish pattern for message types
- Built-in correlation ID management for reliable message tracking

**ServerTransport Pattern:**
- `try_next_message()` method for event-driven message reception
- Multi-connection support with automatic connection multiplexing
- Clean async/await API without manual transport driving
- Envelope-based type-safe message handling

### Network Abstractions

The simulated network types implement the exact same traits as Tokio's networking primitives (`AsyncRead`, `AsyncWrite`, etc.), ensuring compatibility at the I/O level. Connection and Listener types produced by the NetworkProvider work identically whether they're real or simulated. The transport layer sits above these abstractions, providing request-response semantics regardless of the underlying network implementation.

### Time Abstractions

Simulated time operations provide the same interface as `std::time` and Tokio's time utilities, but work with logical time instead of wall-clock time. Sleep operations schedule wake events in the simulation queue rather than blocking on actual time passage, enabling deterministic and fast test execution.

## Simulation Report System

### Report API and Metrics Collection

The framework provides comprehensive reporting and metrics collection for analyzing simulation behavior across multiple runs. The `SimulationReport` captures detailed statistics and behavioral patterns to help understand system performance under various conditions.

**Core API Pattern:**
```rust
let report = SimulationBuilder::new()
    .set_iterations(100)
    .register_workload("ping_pong", |seed| async move {
        // Test logic here
        let provider = get_network_provider(seed);
        run_ping_pong_test(provider).await
    })
    .register_workload("consensus", |seed| async move {
        // Another test scenario
        run_consensus_test(seed).await
    })
    .run().await;

// Analyze results
println!("Total simulated time: {:?}", report.total_simulated_time());
println!("Iterations completed: {}", report.iterations());
println!("Success rate: {:.2}%", report.success_rate());
```

### SimulationReport Structure

The `SimulationReport` provides comprehensive metrics and analysis:

**Core Metrics:**
- **Total Wall Time**: Real time spent running all iterations
- **Total Simulated Time**: Sum of logical time across all iterations  
- **Iteration Count**: Number of completed simulation runs
- **Success Rate**: Percentage of iterations that completed successfully
- **Failure Analysis**: Detailed breakdown of failure modes and their frequencies


### Workload Registration System

The framework supports registering multiple async workloads that can be executed across different seeds and iterations:

**Workload Interface:**
```rust
pub trait WorkloadFunction: Fn(u64) -> Pin<Box<dyn Future<Output = WorkloadResult> + Send>> {}

pub struct SimulationBuilder {
    iterations: usize,
    workloads: Vec<(&'static str, Box<dyn WorkloadFunction>)>,
    seed_strategy: SeedStrategy,
}

impl SimulationBuilder {
    pub fn register_workload<F, Fut>(self, name: &'static str, workload: F) -> Self 
    where
        F: Fn(u64) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = WorkloadResult> + Send + 'static,
    {
        // Register async closure for execution
    }
}
```

**Workload Execution Model:**
- Each workload receives a unique seed for deterministic randomness
- Workloads are executed in isolation with fresh simulation state
- Failed workloads are captured in the report with detailed error information
- Workloads can be combined and executed in different orders

### Multiple Seeds Execution

The framework automatically executes workloads across multiple seeds to comprehensively test system behavior:

**Seed Management:**
```rust
pub enum SeedStrategy {
    Random(usize),                      // Generate N random seeds
}

// Example usage
let report = SimulationBuilder::new()
    .set_iterations(1000)
    .seed_strategy(SeedStrategy::Random(1000))
    .register_workload("distributed_consensus", |seed| async move {
        // Test runs with different seed each iteration
        run_consensus_with_seed(seed).await
    })
    .run().await;
```

**Parallel Execution:**
- Seeds can be executed in parallel across multiple threads
- Each seed gets an isolated simulation environment
- Results are aggregated into a comprehensive report
- Failed seeds are retried with detailed logging

### Thread-Local RNG Design

The framework uses thread-local random number generation for better performance and cleaner API design:

**Thread-Local RNG Architecture:**
```rust
thread_local! {
    static SIM_RNG: RefCell<ChaCha8Rng> = RefCell::new(ChaCha8Rng::from_entropy());
}

pub fn sim_random<T>() -> T 
where 
    StandardDistribution: Distribution<T>,
{
    SIM_RNG.with(|rng| rng.borrow_mut().gen())
}

pub fn set_sim_seed(seed: u64) {
    SIM_RNG.with(|rng| {
        *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(seed);
    });
}
```

**Benefits:**
- No need to pass RNG through the simulation state
- Cleaner API without explicit RNG parameters
- Thread-safe by design (each thread has its own RNG)
- Deterministic behavior within each thread/iteration
- Compatible with single-core simulation execution model

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

## Buggify Chaos Testing System

### Randomized Fault Injection

Inspired by FoundationDB's Buggify system, the framework provides randomized chaos testing that automatically injects faults at strategic points in the code. This helps discover edge cases and race conditions that would be difficult to test manually.

**Buggify API:**
```rust
// Probabilistic fault injection
if buggify!(0.1) {  // 10% chance
    // Inject network delay
    sim_sleep(Duration::from_millis(sim_random_range(100..1000))).await;
}
```

**Buggify Categories:**
- **Network Buggify**: Random packet loss, delays, connection failures
- **Timing Buggify**: Unexpected delays, early timeouts, clock skew
- **Resource Buggify**: Memory pressure, file descriptor limits
- **Logic Buggify**: State corruption, invalid messages, protocol violations

**Integration with Thread-Local RNG:**
Buggify uses the thread-local RNG system for deterministic randomization. The same seed will always produce the same sequence of buggify decisions, ensuring reproducible test failures.

```rust
// Buggify implementation using thread-local RNG
pub fn buggify(probability: f64) -> bool {
    debug_assert!(probability >= 0.0 && probability <= 1.0);
    sim_random::<f64>() < probability
}

pub fn buggify_range<T>(min: T, max: T) -> T 
where 
    T: SampleUniform,
{
    sim_random_range(min..max)
}
```

**Buggify Configuration:**
```rust
pub struct BuggifyConfig {
    pub enabled: bool,
}
```

**Strategic Injection Points:**
Buggify should be placed at critical junctions where faults would expose the most bugs:
- Before/after network operations
- During state transitions
- At synchronization points
- During resource allocation/deallocation
- At protocol boundaries

### Chaos Engineering Integration

The Buggify system integrates with the simulation framework to provide comprehensive chaos testing:

**Automatic Fault Discovery:**
- Continuously running tests with increasing fault rates
- Automatic detection of new failure modes
- Minimal reproduction of discovered failures
- Integration with the SimulationReport for failure analysis


## Assertions System

The framework provides both traditional assertions and probabilistic "sometimes" assertions.

**Basic API:**
```rust
// Traditional assertion - must always be true
always_assert!(leader_elected, {
    current_leader.is_some()
}, "A leader must always be elected");

// Sometimes assertion - should be true in some percentage of cases
sometimes_assert!(consensus_reached_quickly, {
    consensus_time < Duration::from_secs(5)
}, "Consensus should complete quickly in normal conditions");
```

**Macro implementations:**
```rust
macro_rules! always_assert {
    ($name:expr, $condition:expr, $description:expr) => {
        {
            let result = $condition;
            // Record assertion result in simulation report
            if let Some(report) = current_simulation_report() {
                report.record_always_assertion($name, result, $description);
            }
            if !result {
                panic!("Assertion failed: {} - {}", $name, $description);
            }
            result
        }
    };
}

macro_rules! sometimes_assert {
    ($name:expr, $condition:expr, $description:expr) => {
        {
            let result = $condition;
            // Record assertion result in simulation report for statistical analysis
            if let Some(report) = current_simulation_report() {
                report.record_sometimes_assertion($name, result, $description);
            }
            result
        }
    };
}
```

## Implementation Status

### Core Infrastructure (✅ COMPLETED)
- ✅ Logical time engine with event-driven advancement
- ✅ Event queue and deterministic processing
- ✅ Thread-local deterministic RNG with seed-based reproduction
- ✅ Comprehensive simulation harness and builder pattern

### Network Simulation (✅ COMPLETED)
- ✅ Point-to-point connection simulation with realistic lifecycle
- ✅ Message delivery simulation with configurable conditions
- ✅ Comprehensive fault injection (delays, drops, partitions)
- ✅ Connection failure simulation and recovery testing

### Tokio Integration (✅ COMPLETED)
- ✅ AsyncRead/AsyncWrite implementations for simulated networking
- ✅ TcpStream/TcpListener simulation with full compatibility
- ✅ Time APIs (sleep, timeout, Instant) with logical time
- ✅ Provider pattern for seamless simulation/real network switching

### Transport Layer (✅ COMPLETED - Phase 11)
- ✅ Sans I/O protocol architecture with complete separation of concerns
- ✅ Envelope-based messaging with correlation ID tracking
- ✅ Self-driving ClientTransport and ServerTransport APIs
- ✅ Binary serialization with length-prefixed wire format
- ✅ Comprehensive chaos testing integration with deterministic fault injection

### Testing Framework (✅ COMPLETED)
- ✅ Intuitive multi-seed testing infrastructure with statistical analysis
- ✅ Buggify chaos testing system with FoundationDB-inspired fault injection
- ✅ Always/sometimes assertion system for comprehensive validation
- ✅ Hostile infrastructure defaults with extreme failure conditions
- ✅ Simulation reporting with detailed coverage analysis
- ✅ Deterministic runtime creation per seed iteration
- ✅ Statistical validation across multiple test runs

### Advanced Features (✅ COMPLETED)
- ✅ Network partitions and asymmetric connectivity scenarios
- ✅ Message reordering and duplication simulation
- ✅ Connection failure simulation with automatic recovery
- ✅ Performance optimization for large-scale simulations
- ✅ Multi-client/multi-server testing with connection multiplexing

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