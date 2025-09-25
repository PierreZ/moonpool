# Simulation Core Framework Specification

## Overview

The Moonpool Simulation Core provides deterministic, reproducible simulation environment for testing distributed systems. This specification describes the foundational infrastructure that enables seamless switching between simulated and real implementations while maintaining identical application logic.

## Design Philosophy

**Primary Goal: Seamless Runtime Switching**

Applications depend on provider traits (like `NetworkProvider`, `TimeProvider`) rather than concrete implementations. This enables switching between simulated and real implementations by changing the provider, allowing the same application logic to run in production with real networking and in tests with controlled simulation.

**Deterministic Execution:**
All runs with the same seed produce identical behavior, enabling reliable reproduction of bugs and systematic exploration of edge cases through multi-seed testing.

## Core Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Application Code                         │
│            (Same logic, different providers)               │
├─────────────────────────────────────────────────────────────┤
│                 Provider Traits                            │
│  ┌─────────────────┐┌─────────────────┐┌─────────────────┐ │
│  │ NetworkProvider ││  TimeProvider   ││  TaskProvider   │ │
│  │                 ││                 ││                 │ │
│  │ • connect()     ││ • sleep()       ││ • spawn_task()  │ │
│  │ • listen()      ││ • timeout()     ││ • Local runtime │ │
│  │ • Provider-     ││ • Instant       ││ • Single-core   │ │
│  │   agnostic API  ││ • Duration      ││   execution     │ │
│  └─────────────────┘└─────────────────┘└─────────────────┘ │
├─────────────────────────────────────────────────────────────┤
│              Implementation Selection                       │
│  ┌───────────────────────┐  ┌─────────────────────────────┐ │
│  │   Simulation Mode     │  │      Production Mode       │ │
│  │                       │  │                             │ │
│  │ • SimNetworkProvider  │  │ • TokioNetworkProvider     │ │
│  │ • SimTimeProvider     │  │ • TokioTimeProvider        │ │
│  │ • SimTaskProvider     │  │ • TokioTaskProvider        │ │
│  │ • Logical time        │  │ • Wall clock time          │ │
│  │ • Event-driven        │  │ • Real I/O operations      │ │
│  │ • Deterministic       │  │ • System resources         │ │
│  └───────────────────────┘  └─────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

## Core Components

### 1. Simulation World (`SimWorld`)

The central coordinator that maintains all simulation state and ensures deterministic execution.

**Primary Responsibilities:**
- Global simulation clock management with logical time advancement
- Event queue coordination and deterministic event processing
- State registry management for all simulation entities
- Deterministic random number generation coordination
- Cross-component synchronization and coordination

**Ownership Architecture:**
- Single owner of all mutable simulation state
- Handle-based access pattern with `{ WeakSim, Id }` for components
- Short-lived borrows to prevent nested borrowing conflicts
- Event-driven state management to avoid cross-module dependencies

### 2. Event System

**Discrete Event Simulation:**
The simulation operates on discrete events scheduled for future execution, ensuring deterministic ordering and reproducible behavior.

**Event Types:**
- **Wake Events**: Resume sleeping tasks at specific logical times
- **Network Events**: Message delivery, connection establishment/teardown
- **Fault Events**: Deterministic fault injection based on buggify configuration
- **Infrastructure Events**: System maintenance tasks (connection cleanup, etc.)

**Event Processing:**
1. Pop earliest event from priority queue (deterministic ordering)
2. Advance logical time to event timestamp
3. Execute event handler with full context access
4. Process any immediately scheduled follow-up events
5. Continue until termination condition or queue exhaustion

**Termination Logic:**
- Infrastructure event detection prevents infinite loops
- Workload completion detection with early termination
- Timeout mechanisms for bounded test execution
- Graceful cleanup and resource management

### 3. Time Engine

**Logical Time Management:**
Provides deterministic time simulation using logical time that advances only when events are processed, eliminating wall-clock dependency for reproducible timing behavior.

**Key Features:**
- Event-driven time advancement (no wall-clock dependency)
- Compatible with `std::time::Instant` and `Duration` APIs  
- Deterministic sleep/timeout behavior through event scheduling
- Time dilation support for race condition exposure
- Integration with provider pattern for seamless switching

**Time Provider Interface:**
- `sleep(duration)`: Schedule wake event in simulation queue
- `timeout(future, duration)`: Bounded future execution with logical timing
- `now()`: Current logical time from simulation clock
- `elapsed()`: Logical duration calculation for timing measurements

### 4. Deterministic Random Engine

**Seed-Based Randomness:**
Provides reproducible random behavior across simulation runs using thread-local ChaCha8Rng with deterministic seeding.

**Thread-Local Design Benefits:**
- No need to pass RNG through simulation state for cleaner APIs
- Thread-safe by design with isolated per-thread state
- Compatible with single-core simulation execution model
- Deterministic behavior within each thread/iteration
- Clean integration with buggify chaos testing system

**Multi-Seed Testing Integration:**
- Support for running tests with multiple different seeds
- Comprehensive edge case exploration through random variation
- Each seed iteration uses isolated runtime with specific seeding
- Statistical analysis across multiple seed runs for validation

**RandomProvider Trait:**
- Abstract random generation through provider pattern
- Simulation and real implementations for different execution modes
- Type-safe random generation with range constraints
- Integration with workload functions for deterministic testing

### 5. Provider Pattern Infrastructure

**Core Abstraction Strategy:**
Applications depend on abstract provider traits rather than concrete implementations, enabling compile-time or runtime switching between simulation and production environments.

**Provider Categories:**

**NetworkProvider:**
- Connection establishment and listener creation
- Address resolution and binding management
- Provider-specific configuration and optimization
- Seamless switching between simulated and real networking

**TimeProvider:**  
- Sleep and timeout operations with logical vs wall-clock time
- Instant and Duration compatibility with standard library
- Event scheduling integration for deterministic timing
- Time dilation and manipulation for testing scenarios

**TaskProvider:**
- Local task spawning with single-core runtime optimization
- Task coordination and lifecycle management
- Integration with simulation event system
- Provider-specific concurrency patterns and limitations

**RandomProvider:**
- Deterministic random generation with seed-based reproduction
- Type-safe random value generation with range constraints
- Integration with chaos testing and fault injection
- Statistical distribution support for realistic simulation

## Simulation Runners

### SimulationBuilder

**Workload Registration System:**
- Type-safe async workload registration with provider injection
- Multiple workloads per test suite for comprehensive coverage
- Seed-based parameterization for deterministic reproduction
- Result aggregation and statistical analysis across iterations

**Configuration Management:**
- Iteration count and seed strategy configuration
- Global simulation parameters (timeouts, fault injection rates)
- Provider-specific configuration and optimization settings
- Integration with chaos testing configuration

**Execution Coordination:**
- Parallel execution across multiple seeds for efficiency
- Isolated runtime per seed for complete test isolation
- Progress monitoring and execution status reporting
- Failure isolation and detailed error information capture

### TokioRunner

**Production Deployment Support:**
- Real networking validation with identical application logic
- Performance baseline establishment for simulation comparison
- Integration testing with actual system resources
- Production deployment validation and sanity checking

**Hybrid Testing:**
- Mixed simulation/real network testing scenarios
- Gradual migration from simulation to production validation
- Performance comparison between simulation and real execution
- Integration with CI/CD pipeline for comprehensive validation

## Advanced Features

### Provider Pattern Extensions

**Seamless Switching:**
- Runtime provider selection based on execution context
- Configuration-driven provider selection for flexible deployment
- Mixed provider scenarios for hybrid testing approaches
- Performance comparison across different provider implementations

**Provider Configuration:**
- Per-provider optimization settings and tuning parameters
- Environment-specific configuration management
- Integration with external configuration systems
- Runtime reconfiguration for dynamic testing scenarios

### Event-Driven Architecture

**Coordination Mechanisms:**
- Cross-component event scheduling for complex interactions
- Event priority management for deterministic execution ordering
- Event filtering and routing for targeted component interaction
- Integration with external event sources for hybrid scenarios

**Performance Optimization:**
- Efficient event queue implementation with minimal overhead
- Event batching and processing optimization for large-scale simulation
- Memory management and resource cleanup for long-running simulations
- CPU utilization optimization for single-core execution model

### Integration Points

**Testing Framework Integration:**
- Assertion system integration for validation during simulation
- Chaos testing coordination with deterministic fault injection
- Coverage analysis and statistical validation across simulation runs
- Regression detection through historical simulation comparison

**Transport Layer Coordination:**
- Seamless integration with Sans I/O transport architecture
- Event-driven coordination between simulation and transport layers
- Provider pattern consistency across all framework components
- Performance optimization for transport-simulation integration

## Performance Characteristics

### Memory Efficiency

**Optimized State Management:**
- Minimal per-entity overhead with efficient handle patterns
- Memory pooling and reuse for high-frequency allocations
- Efficient event queue implementation for large-scale simulation
- Proper resource cleanup and leak prevention mechanisms

**Scalability Patterns:**
- Single-core optimization for reduced complexity and overhead
- Efficient data structures for large simulation state management
- Memory usage patterns optimized for extended simulation runs
- Resource utilization monitoring and optimization guidance

### CPU Optimization

**Event Processing Efficiency:**
- Optimized event queue with minimal processing overhead
- Efficient state lookup and manipulation through handle patterns
- CPU usage balanced between simulation accuracy and performance
- Profile-guided optimization for critical simulation paths

**Provider Performance:**
- Minimal overhead for provider trait dispatch
- Optimized simulation provider implementations
- Performance comparison and benchmarking across provider types
- CPU utilization patterns optimized for single-core execution

## Future Enhancements

### Advanced Simulation Features
- Multi-node simulation with distributed simulation state
- Advanced time manipulation including time travel and replay
- Integration with external simulators and hybrid environments
- Enhanced fault injection with machine learning-driven scenarios

### Performance and Scalability
- Multi-core simulation execution with proper synchronization
- Advanced memory management with zero-copy optimizations
- Integration with high-performance networking stacks
- Scalability improvements for large-scale distributed system simulation

### Tooling and Integration
- Advanced debugging and introspection tools for simulation state
- Integration with monitoring and observability frameworks
- Command-line tools for simulation configuration and execution
- IDE integration for simulation development and debugging

## References

- Discrete event simulation principles and best practices
- FoundationDB simulation framework architecture and lessons learned
- Provider pattern implementation strategies for Rust
- Single-core async runtime optimization techniques