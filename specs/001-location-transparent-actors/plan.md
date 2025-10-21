# Implementation Plan: Location-Transparent Distributed Actor System

**Branch**: `001-location-transparent-actors` | **Date**: 2025-10-21 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/001-location-transparent-actors/spec.md`

## Summary

Build a distributed virtual actor system where developers obtain actor references by ID without knowing node locations. The system automatically activates actors on-demand, routes messages across nodes via a distributed directory, processes messages sequentially with single-threaded guarantees per actor, and handles node failures. Core experience matches Orleans' location transparency: define actors, implement async methods, call like normal functions while the framework handles activation/deactivation, directory registration, message routing, request-response correlation, and timeout enforcement.

**Technical Approach**: Custom actor-specific messaging protocol built on moonpool-foundation's PeerTransport, with MessageBus for correlation-based request-response, ActorCatalog for lifecycle management using explicit state machines, and eventually-consistent Directory for actor placement using two-random-choices load balancing. All components use Provider pattern for deterministic simulation testing.

## Technical Context

**Language/Version**: Rust 1.75+ (stable, async/await support required)
**Primary Dependencies**:
- `tokio` (async runtime, single-threaded: `Builder::new_current_thread().build_local()`)
- `serde` + `serde_json` (serialization, first implementation)
- `moonpool-foundation` (PeerTransport, SimulationRuntime, Buggify, providers)

**Storage**: N/A (actors are stateless in-memory, no persistence in this phase)
**Testing**:
- `cargo nextest` with deterministic simulation
- Multi-topology tests (1x1, 2x2, 10x10 clusters)
- Chaos testing via Buggify with hostile conditions
- Invariant validation after every simulation event

**Target Platform**: Linux server (development), deterministic simulation (testing)
**Project Type**: Library crate (`moonpool/`) within workspace
**Performance Goals**:
- Actor reference retrieval: <100ms (typical case)
- Message delivery: 100% in static cluster
- Concurrent operations: 1000+ requests without race conditions
- Directory placement: balanced distribution within 20% variance for 100+ actors

**Constraints**:
- Single-threaded execution (no Send/Sync requirements)
- Static cluster topology (no dynamic membership)
- No network partitions or node failures in this phase
- Provider pattern for all I/O (deterministic simulation)
- No LocalSet usage (forbidden pattern)
- No unwrap()/expect() in production code

**Scale/Scope**:
- Support clusters of 1-10+ nodes
- Handle 100+ concurrent actors per node
- Process 1000+ messages without deadlocks
- Test under 10x worse conditions than production

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

### Principle I: Determinism Over Convenience
✅ **PASS** - All async operations use provider abstractions:
- TimeProvider for sleep/timeout (not tokio::time)
- TaskProvider for spawn (not tokio::spawn)
- NetworkProvider for I/O (through moonpool-foundation)
- Same seed produces identical behavior

### Principle II: Explicitness Over Implicitness
✅ **PASS** - All error handling explicit:
- Result<T, E> for all fallible operations
- Error propagation via ? operator
- No unwrap()/expect() in production code
- Descriptive error types with context

### Principle III: Single-Core Simplicity
✅ **PASS** - No Send/Sync requirements:
- #[async_trait(?Send)] for all async traits
- Builder::new_current_thread().build_local() for runtime
- No LocalSet wrapper (forbidden)
- Single-threaded execution throughout

### Principle IV: Trait-Based Design
✅ **PASS** - Depend on traits, not concrete types:
- Actor behaviors defined via traits
- Provider pattern for Time/Network/Task/Random
- Public APIs accept trait bounds
- Core abstractions (MessageBus, Directory, ActorCatalog) are traits

### Principle V: State Machine Clarity
✅ **PASS** - Explicit enum-based state machines:
- Actor lifecycle: Creating → Activating → Valid → Deactivating → Invalid
- Message direction: Request | Response | OneWay
- Directory operations have clear state transitions
- No boolean soup for complex state

### Principle VI: Comprehensive Chaos Testing
✅ **PASS** - Hostile simulation conditions:
- Multi-topology tests (1x1, 2x2, 10x10)
- Buggify for concurrent activation races, delays, message reordering
- always_assert! for invariants (single activation, balance consistency)
- sometimes_assert! for normal behaviors (100% coverage target)
- Cross-workload invariant validation

### Principle VII: Simplicity First (KISS)
✅ **PASS** - Start simple, add complexity when justified:
- Excludes persistence, transactions, migration, streams (out of scope)
- Eventual consistency for directory (simpler than strong consistency)
- serde_json first (human-readable, simple) before binary formats
- Custom protocol only where generic foundation protocol insufficient

**Overall Gate Status**: ✅ **PASS** - All principles satisfied, no violations to justify

## Project Structure

### Documentation (this feature)

```
specs/001-location-transparent-actors/
├── plan.md              # This file (/speckit.plan command output)
├── research.md          # Phase 0 output (/speckit.plan command)
├── data-model.md        # Phase 1 output (/speckit.plan command)
├── quickstart.md        # Phase 1 output (/speckit.plan command)
├── contracts/           # Phase 1 output (/speckit.plan command)
│   ├── actor.rs         # Actor trait definitions and contracts
│   ├── message.rs       # Message structure and protocol contracts
│   └── directory.rs     # Directory service contracts
└── tasks.md             # Phase 2 output (/speckit.tasks command - NOT created by /speckit.plan)
```

### Source Code (repository root)

```
moonpool/                          # New crate for actor system
├── Cargo.toml                     # Dependencies: tokio, serde, serde_json, moonpool-foundation
├── src/
│   ├── lib.rs                     # Public API exports
│   ├── actor/
│   │   ├── mod.rs                 # Actor trait and ActorContext
│   │   ├── lifecycle.rs           # ActivationState state machine
│   │   └── catalog.rs             # ActorCatalog (local actor registry)
│   ├── messaging/
│   │   ├── mod.rs                 # Message struct, Direction enum, MessageFlags
│   │   ├── bus.rs                 # MessageBus (correlation, routing, timeout)
│   │   ├── protocol.rs            # Custom protocol on PeerTransport
│   │   └── correlation.rs         # CallbackData, request-response tracking
│   ├── directory/
│   │   ├── mod.rs                 # Directory trait
│   │   ├── simple.rs              # Simple in-memory directory (eventual consistency)
│   │   └── placement.rs           # Two-random-choices placement algorithm
│   ├── runtime/
│   │   ├── mod.rs                 # ActorSystem (main entry point)
│   │   └── node.rs                # Node-level coordination
│   └── error.rs                   # ActorError types
│
└── tests/
    ├── simulation/
    │   ├── single_node.rs         # 1x1 cluster tests
    │   ├── two_node.rs            # 2x2 cluster tests
    │   ├── multi_node.rs          # 10x10 cluster tests
    │   └── chaos.rs               # Buggify-enabled chaos tests
    ├── integration/
    │   ├── bank_account.rs        # BankAccount actor example
    │   └── invariants.rs          # Banking invariants validation
    └── unit/
        ├── lifecycle.rs           # Actor lifecycle state machine tests
        ├── correlation.rs         # Request-response correlation tests
        └── placement.rs           # Directory placement algorithm tests
```

**Structure Decision**: Single library crate (`moonpool/`) within the workspace, separate from `moonpool-foundation/`. The actor system builds on foundation's networking primitives but implements its own domain-specific abstractions. Test organization reflects three levels: unit (component-level), integration (multi-component), and simulation (multi-node deterministic chaos testing).

## Complexity Tracking

*No violations - all Constitution principles satisfied*

This plan requires no deviations from constitutional principles.

## Phase 0: Research & Technical Unknowns

### Unknowns Requiring Research

**NEEDS CLARIFICATION: How does PeerTransport expose its API?**
- Research task: Examine `moonpool-foundation/src/networking/peer_transport.rs`
- Required answers:
  - What trait methods exist for sending/receiving messages?
  - How are connections established between peers?
  - What's the message format expected by the transport layer?
  - How does the transport integrate with Provider pattern?

**NEEDS CLARIFICATION: How does Buggify integrate into custom code?**
- Research task: Examine `moonpool-foundation/src/simulation/buggify.rs` and testing examples
- Required answers:
  - What's the API for injecting chaos points?
  - How do we mark "sometimes fail here" locations?
  - How does simulation runtime track buggify coverage?
  - How to configure chaos intensity per test?

**NEEDS CLARIFICATION: What's the trait signature for Provider patterns?**
- Research task: Examine `moonpool-foundation/src/providers/` module
- Required answers:
  - Exact method signatures for `TimeProvider::sleep()`, `timeout()`, etc.
  - How to obtain provider instances in actor code?
  - How are providers injected during construction vs simulation?
  - What's the pattern for passing providers through the call stack?

### Technology Best Practices Research

**Rust async patterns for single-threaded executor**
- Research task: Find best practices for single-threaded Tokio usage
- Focus areas:
  - Proper runtime configuration (`Builder::new_current_thread().build_local()`)
  - Task spawning patterns without LocalSet
  - Avoiding accidental Send bounds in async traits
  - Performance characteristics vs multi-threaded

**Eventual consistency patterns in Rust**
- Research task: Find patterns for eventually-consistent distributed data structures
- Focus areas:
  - Cache invalidation strategies
  - Version vectors or logical clocks
  - Read-your-writes consistency guarantees
  - Convergence testing strategies

**State machine patterns in Rust**
- Research task: Find best practices for enum-based state machines
- Focus areas:
  - Type-state pattern for compile-time guarantees
  - Transition validation at runtime
  - State-specific data via enum variants
  - Testing state machine exhaustiveness

## Phase 1: Design Artifacts

### Data Model (data-model.md)

Key entities from spec:
1. **Actor** - Computational entity with ID, type, state, behavior
2. **ActorReference** - Location-transparent handle for message sending
3. **Message** - Communication unit with correlation, direction, routing
4. **Directory** - ActorId → NodeId mapping with placement
5. **Node** - Cluster participant hosting actors
6. **Cluster** - Collection of nodes

State transitions:
- Actor lifecycle: Creating → Activating → Valid → Deactivating → Invalid
- Message flow: Send → Route → Deliver → Process → Respond

### API Contracts (contracts/)

From functional requirements:
- **FR-001**: `ActorSystem::get_actor<T>(id) -> ActorRef<T>` - Obtain actor reference by ID
- **FR-002**: First message triggers automatic activation via `ActorCatalog::get_or_create()`
- **FR-003**: `MessageBus::route(message)` - Route to correct node via directory lookup
- **FR-004**: Sequential processing via per-actor message queue
- **FR-006-007**: `ActorRef::call(request) -> Result<Response>` with timeout
- **FR-008-009**: `Actor::on_activate()`, `Actor::on_deactivate()` lifecycle hooks

Contract files:
- `contracts/actor.rs` - Actor trait, ActorContext, lifecycle hooks
- `contracts/message.rs` - Message struct, Direction enum, MessageFlags
- `contracts/directory.rs` - Directory trait, placement algorithm interface

### Quickstart (quickstart.md)

Minimal BankAccount example demonstrating:
1. Define actor with state and methods
2. Implement lifecycle hooks (activation/deactivation)
3. Start actor system with single-node cluster
4. Obtain actor reference and send messages
5. Request-response pattern with timeout
6. Error handling

## Phase 2: Implementation Tasks

*Generated by `/speckit.tasks` command - not part of this plan output*

Tasks will be ordered by dependency and grouped into logical phases:
1. Core message types and serialization
2. Actor lifecycle state machine
3. Message bus with correlation
4. Directory with placement
5. ActorCatalog with double-check locking
6. Integration and end-to-end tests
7. Multi-topology simulation tests
8. Chaos testing with invariant validation
