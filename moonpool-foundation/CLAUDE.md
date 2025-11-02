# Moonpool Foundation: Deterministic Simulation Framework

## Overview
Foundation crate providing core simulation infrastructure, transport layer, and testing framework. This is the low-level layer that enables deterministic testing of distributed systems, particularly designed for testing the moonpool actor system.

**Status**: ✅ COMPLETE - HOBBY-GRADE

All features implemented and tested, but remains hobby-grade. Suitable for simulation testing but not production systems.

## Core Constraints
- Single-core execution (no Send/Sync)
- No `unwrap()` - use `Result<T, E>` with `?`
- Document all public items
- Networking: `#[async_trait(?Send)]`
- Use traits, not concrete types
- KISS principle
- Use LocalRuntime, not LocalSet (spawn_local without Builder)
- **No LocalSet usage**: Use `tokio::runtime::Builder::new_current_thread().build_local()` only
- **No direct tokio calls**: Use Provider traits (`TimeProvider::sleep()`, not `tokio::time::sleep()`)
  - Forbidden: `tokio::time::sleep()`, `tokio::time::timeout()`, `tokio::spawn()`
  - Required: `time.sleep()`, `time.timeout()`, `task_provider.spawn_task()`

## Testing Philosophy
**Goal**: 100% sometimes assertion coverage via chaos testing + comprehensive invariant validation
**Target**: 100% success rate - no deadlocks/hangs acceptable

**Multi-seed testing**: Default `UntilAllSometimesReached(1000)` runs until all sometimes_assert! statements have triggered
**Failing seeds**: Debug with `SimulationBuilder::set_seed(failing_seed)` → fix root cause → verify → re-enable chaos
**Infrastructure events**: Tests terminate early when only ConnectionRestore events remain to prevent infinite loops
**Invariant checking**: Cross-workload properties validated after every simulation event
**Goal**: Find bugs, not regression testing

## Assertions & Buggify
**Always**: Guard invariants (never fail)
**Sometimes**: Test error paths (statistical coverage)
**Buggify**: Deterministic fault injection (25% probability when enabled)

Strategic placement: error handling, timeouts, retries, resource limits

## API Design Principles
**Keep APIs familiar**: APIs should match what developers expect from tokio objects
- Maintain async patterns where developers expect them
- Use standard tokio types and conventions
- Avoid surprises in API behavior vs real networking

## Architecture Layers

### 1. Core Simulation (Phases 1-2)
**SimWorld**: Central coordinator maintaining simulation state
- Logical time engine with event-driven advancement
- Event queue and deterministic processing
- Handle pattern with weak references (avoid borrow conflicts)
- Thread-local deterministic RNG with seed-based reproduction

**Provider Pattern**: Trait-based abstraction for swapping implementations
- `NetworkProvider` - Simulated vs real networking
- `TimeProvider` - Logical time vs wall-clock time
- `TaskProvider` - Local task spawning
- `RandomProvider` - Deterministic vs real randomness

### 2. Network Simulation (Phases 2-7)
**Peer Networking**: Low-level TCP connection management
- Point-to-point connection simulation
- AsyncRead/AsyncWrite implementations
- TcpStream/TcpListener simulation with full compatibility
- Connection lifecycle: establishment, failures, recovery
- Automatic reconnection with exponential backoff

**Fault Injection Engine**: Configurable chaos
- Packet loss, delays, and partitions
- Connection failure simulation
- Network partition scenarios
- Hostile infrastructure defaults (10x worse than production)

**Code mapping to FoundationDB**:
- Peer → FlowTransport.h:147-191, FlowTransport.actor.cpp:1016-1125
- Connection → FlowTransport.actor.cpp:760-900
- Backoff → FlowTransport.actor.cpp:892-897
- Reliability → Net2Packet.h:30-111

### 3. Transport Layer (Phase 11 - COMPLETED)
**Sans I/O Architecture**: Protocol logic separated from I/O operations
- **Envelope System**: Type-safe messaging with correlation IDs for request-response semantics
- **Self-Driving Futures**: `request()` and `try_next_message()` internally manage transport state
- **Binary Wire Format**: Length-prefixed serialization with `InsufficientData` error handling
- **Multi-Connection Support**: Server handles multiple concurrent connections with automatic multiplexing

**Key APIs**:
- `ClientTransport::request::<TReq, TResp>(msg)` - Type-safe request-response with automatic correlation
- `ServerTransport::try_next_message()` - Event-driven message reception across all connections
- `EnvelopeSerializer` - Binary serialization with partial read support
- `RequestResponseEnvelopeFactory` - Correlation ID management for reliable message tracking

**Developer Experience**: FoundationDB-inspired patterns with 70% code reduction
- Single `tokio::select!` patterns replace complex manual loops
- Clean async/await without manual `tick()` calls
- Result-based error handling from message streams

### 4. Testing Framework (Phases 8-11)
**Buggify System**: FoundationDB-style deterministic fault injection
- `buggify!(probability)` - Probabilistic fault triggers
- Strategic placement: error handling, state transitions, resource operations
- Deterministic via thread-local RNG

**Assertions System**:
- `always_assert!` - Invariants that must never be violated
- `sometimes_assert!` - Behaviors that should occur under normal conditions
- Coverage tracking across multiple seeds

**Invariant System**: Cross-workload validation
- JSON-based state registry
- Global property validation after every simulation event
- 7 comprehensive bug detectors:
  - Message conservation (sent = received + in_transit)
  - Per-peer message accounting
  - Connection state consistency
  - Correlation ID uniqueness
  - Queue state validation
  - Lifecycle ordering
  - Load distribution fairness

**Multi-Topology Testing**:
- 1x1 (basic functionality)
- 1x2, 1x10 (asymmetric loads)
- 2x2 (distributed scenarios)
- 10x10 (stress testing)

## Reference Documentation

### FoundationDB References (Always Read First!)
**Location**: `docs/references/foundationdb/`

**When working on simulation core**:
- Read `sim2.actor.cpp` (SimWorld equivalent)
- Read `Net2.actor.cpp` (event processing)

**When working on networking**:
- Read `FlowTransport.h` and `FlowTransport.actor.cpp` (Peer, Connection)
- Read `Net2Packet.h` (reliability, queuing)

**When working on chaos testing**:
- Read `Buggify.h` (chaos injection patterns)
- Read `Ping.actor.cpp` (example workload with assertions)

### Analysis Documents
**Location**: `docs/analysis/foundationdb/`

- `flow.md` - **READ THIS FIRST** before touching `actor.cpp` code
- `fdb-network.md` - Network architecture deep dive

### TigerBeetle Reference
**Location**: `docs/references/tigerbeetle/`
- `packet_simulator.zig` - Network fault configuration patterns

### Specifications
**Location**: `docs/specs/`

- `moonpool-foundation-spec.md` - Framework overview and architecture
- `simulation-core-spec.md` - Core simulation infrastructure
- `transport-layer-spec.md` - Sans I/O transport architecture
- `peer-networking-spec.md` - TCP connection management
- `testing-framework-spec.md` - Chaos testing and assertion system

### Implementation History
The framework was developed incrementally with comprehensive testing at each stage. All features are now complete but remain hobby-grade.

## Validation Checklist
Before completing any work on foundation:
1. `nix develop --command cargo fmt` - passes
2. `nix develop --command cargo clippy` - no warnings
3. `nix develop --command cargo nextest run` - all tests pass
4. No test timeouts or hangs
5. All sometimes_assert! triggered in chaos tests
6. Invariants validated across all topologies
7. Documentation updated for public APIs

## Notes
- **Hobby-grade project**: Suitable for simulation testing but not production systems
- Focus: TCP-level simulation (connection faults) not packet-level
- Single-core design simplifies reasoning about determinism
- Provider traits enable seamless sim/real switching
- Invariant system catches bugs that assertions miss
- Chaos testing finds edge cases during development
