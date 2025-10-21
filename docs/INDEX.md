# Moonpool Documentation Index

Complete listing of all documentation files with descriptions and cross-references.

## Quick Navigation

- **New to Moonpool?** Start with [specs/moonpool-foundation-spec.md](#specifications)
- **Building on foundation?** Check [analysis/foundationdb/](#foundationdb-analysis)
- **Working on actors?** See [analysis/orleans/](#orleans-analysis)
- **Implementing a phase?** Browse [plans/](#implementation-plans)
- **Need reference code?** Explore [references/](#reference-source-code)

---

## Specifications

High-level technical architecture and design documents.

### [moonpool-foundation-spec.md](specs/moonpool-foundation-spec.md)
**Overview of entire framework** - Start here for architectural understanding
- Design goals and philosophy
- Core components (SimWorld, engines, providers)
- Ownership model and handle pattern
- Event system architecture
- Tokio compatibility layer
- Simulation report system
- Buggify chaos testing
- Assertions system (always/sometimes)
- Open questions and trade-offs

**Links to**: All other specs for detailed component descriptions

---

### [simulation-core-spec.md](specs/simulation-core-spec.md)
**Core simulation infrastructure and event processing**
- SimWorld coordination
- Logical time engine
- Event queue and scheduling
- Deterministic RNG with thread-local design
- Handle pattern for ownership
- Provider pattern architecture
- Configuration and seed management

**References**:
- FoundationDB: `sim2.actor.cpp`, `Net2.actor.cpp`
- Analysis: [foundationdb/flow.md](#flowmd)

---

### [transport-layer-spec.md](specs/transport-layer-spec.md)
**Sans I/O transport layer with request-response semantics**
- Envelope system for type-safe messaging
- Correlation ID management
- Self-driving futures pattern
- Binary wire format (length-prefixed)
- ClientTransport and ServerTransport APIs
- Multi-connection support
- Chaos integration

**References**:
- FoundationDB: `FlowTransport.actor.cpp`, `FlowTransport.h`
- Analysis: [foundationdb/fdb-network.md](#fdb-networkmd)

---

### [peer-networking-spec.md](specs/peer-networking-spec.md)
**Low-level TCP connection management and abstractions**
- Peer connection lifecycle
- Automatic reconnection with exponential backoff
- AsyncRead/AsyncWrite implementations
- TcpStream/TcpListener simulation
- NetworkProvider trait pattern
- Connection failure handling

**References**:
- FoundationDB: `FlowTransport.actor.cpp:760-900` (Connection), `Net2Packet.h` (Reliability)
- Analysis: [foundationdb/fdb-network.md](#fdb-networkmd)

---

### [testing-framework-spec.md](specs/testing-framework-spec.md)
**Chaos testing, assertions, and multi-seed infrastructure**
- Buggify system for deterministic fault injection
- Always/sometimes assertion macros
- Invariant checking system (JSON-based state registry)
- Multi-seed testing with UntilAllSometimesReached
- Multi-topology testing (1x1, 2x2, 10x10, etc.)
- SimulationReport and statistical analysis
- 7 comprehensive bug detectors

**References**:
- FoundationDB: `Buggify.h`, `Ping.actor.cpp`
- TigerBeetle: `packet_simulator.zig`

---

## Implementation Plans

Phase-by-phase roadmaps with detailed implementation steps.

### Foundation Layer (Phases 1-11) - ✅ COMPLETED

#### [phase-1-implementation.md](plans/phase-1-implementation.md)
**Core simulation infrastructure**
- SimWorld and event queue
- Logical time engine
- Basic event processing
- Handle pattern setup

---

#### [phase-2a-network-traits.md](plans/phase-2a-network-traits.md)
**Network provider abstraction**
- NetworkProvider trait definition
- Simulated vs real network switching
- Initial API design

---

#### [phase-2b-simulation.md](plans/phase-2b-simulation.md)
**Network simulation engine**
- Connection registry
- Message delivery simulation
- Address resolution

---

#### [phase-2c-configurable-latency.md](plans/phase-2c-configurable-latency.md)
**Network delay configuration**
- Latency models (constant, uniform, exponential)
- Jitter simulation
- Delay scheduling

---

#### [phase-2d-event-processing.md](plans/phase-2d-event-processing.md)
**Event-driven architecture refinement**
- Event types and handlers
- Priority queue processing
- Time advancement logic

---

#### [phase-2e-ping-pong.md](plans/phase-2e-ping-pong.md)
**First distributed workload**
- Ping-pong test implementation
- Basic message exchange
- Initial validation

---

#### [phase-3-simulation-reports.md](plans/phase-3-simulation-reports.md)
**Reporting and metrics**
- SimulationReport structure
- Statistical analysis
- Multi-seed execution framework

---

#### [phase-4-resilient-peer.md](plans/phase-4-resilient-peer.md)
**Connection resilience**
- Automatic reconnection
- Exponential backoff
- Connection lifecycle management

---

#### [phase-5-unreachable-code-detection.md](plans/phase-5-unreachable-code-detection.md)
**Reliability improvements**
- Detecting unreachable code paths
- Improving error handling
- Dead code elimination

---

#### [phase-6-tcp-ordering-fix.md](plans/phase-6-tcp-ordering-fix.md)
**TCP ordering guarantees**
- Message ordering preservation
- In-order delivery enforcement
- Queue management fixes

---

#### [phase-7-network-disruption.md](plans/phase-7-network-disruption.md)
**Fault injection**
- Network partitions
- Connection failures
- Packet loss simulation

---

#### [phase-8-fix-ping-pong-chaos.md](plans/phase-8-fix-ping-pong-chaos.md)
**Chaos testing refinement**
- Fixing chaos-induced failures
- Improving fault tolerance
- Validation under hostile conditions

---

#### [phase-9-buggify.md](plans/phase-9-buggify.md)
**FoundationDB-style chaos system**
- Buggify macro implementation
- Strategic injection points
- Deterministic randomization
- Integration with thread-local RNG

---

#### [phase-10-actor-peer.md](plans/phase-10-actor-peer.md)
**Actor-style peer API**
- Clean async/await patterns
- Simplified connection management
- Message-oriented API

---

#### [phase-11-net-transport.md](plans/phase-11-net-transport.md)
**Sans I/O transport layer** - Final foundation phase
- Envelope system design
- Correlation ID tracking
- Self-driving futures
- Multi-connection server support
- Comprehensive chaos testing
- Multi-topology validation

---

### Actor System (Phase 12+) - 🚧 IN PROGRESS

#### [phase-12-bootstrap-moonpool.md](plans/phase-12-bootstrap-moonpool.md)
**Actor system bootstrap** - Current phase
- ActorCatalog with lifecycle (Steps 1-2)
- Actor base trait and interface (Step 3)
- Actor inbox with message queue (Step 4)
- Message processing state machine (Step 5)
- MessageBus local routing (Step 6)
- Request/response pattern (Step 7)
- Virtual actor lifecycle (Step 8)

Each step includes:
- State machine definition
- Invariants and bug detectors
- Assertions and buggify placement
- StateRegistry exposure
- Test scenarios and topologies
- Orleans reference mapping

---

## Analysis Documents

Deep dives into reference architectures from production systems.

### FoundationDB Analysis

#### [flow.md](analysis/foundationdb/flow.md)
**READ THIS FIRST before touching actor.cpp code**
- Flow actor model fundamentals
- Async/await patterns in C++
- State machine transformations
- Control flow analysis
- Critical for understanding FDB references

---

#### [fdb-network.md](analysis/foundationdb/fdb-network.md)
**Network architecture deep dive**
- FlowTransport architecture
- Peer connection management
- Connection state machines
- Packet reliability mechanisms
- Message queuing strategies

**Cross-references**:
- [peer-networking-spec.md](#peer-networking-specmd)
- [transport-layer-spec.md](#transport-layer-specmd)
- References: `FlowTransport.actor.cpp`, `FlowTransport.h`

---

### Orleans Analysis

#### [activation-lifecycle.md](analysis/orleans/activation-lifecycle.md)
**Actor activation and deactivation patterns**
- Grain activation process
- Lifecycle state transitions
- Deactivation triggers
- Resource cleanup

**Relevant for**: Phase 12 Steps 1-3, 8
**References**: `ActivationData.cs`, `DeactivationReason.cs`

---

#### [message-system.md](analysis/orleans/message-system.md)
**Messaging architecture in Orleans**
- Message structure and routing
- Correlation ID management
- Request-response patterns
- Message center architecture

**Relevant for**: Phase 12 Steps 5-7
**References**: `Message.cs`, `MessageCenter.cs`, `CallbackData.cs`

---

#### [task-scheduling.md](analysis/orleans/task-scheduling.md)
**Message processing and scheduling**
- Work item queue management
- Task scheduling strategies
- Concurrency control
- Queue prioritization

**Relevant for**: Phase 12 Step 4
**References**: `WorkItemGroup.cs`, `ActivationTaskScheduler.cs`

---

#### [configuration-policies.md](analysis/orleans/configuration-policies.md)
**Configuration patterns and policies**
- Grain collection options
- Timeout configurations
- Lifecycle policies
- System tuning

**Relevant for**: Phase 12 Step 8
**References**: `GrainCollectionOptions.cs`

---

#### [silo-bootstrap.md](analysis/orleans/silo-bootstrap.md)
**Silo bootstrap and SystemTarget infrastructure**
- Lifecycle-based actor runtime initialization
- Lifecycle stages (RuntimeInitialize → Active)
- Lifecycle participant pattern
- SystemTargets as static infrastructure actors
- GrainServices with consistent ring participation
- Ordered execution via WorkItemGroup
- Symmetric shutdown and cleanup

**Relevant for**: Phase 12 Steps 1-2 (ActorCatalog lifecycle, bootstrap)
**References**: `Silo.cs`, `SystemTarget.cs`, `GrainService.cs`, `ISiloLifecycle.cs`

---

## Reference Source Code

Production-quality code from established systems.

### FoundationDB References

Located in `references/foundationdb/`:

#### Simulation Core
- **sim2.actor.cpp** - SimWorld equivalent, event processing
- **Net2.actor.cpp** - Network event handling and I/O

#### Networking
- **FlowTransport.h** - Transport layer interface, Peer definition
- **FlowTransport.actor.cpp** - Transport implementation, Connection (lines 760-900), Peer (lines 1016-1125)
- **Net2Packet.h** - Reliability (lines 30-111), Queuing (lines 43-91)
- **Net2Packet.cpp** - Packet handling implementation

#### Chaos Testing
- **Buggify.h** - Chaos injection macros (lines 79-88)
- **Ping.actor.cpp** - Example workload with assertions (lines 29-38, 147-169)

**Code Mapping to Moonpool**:
- `sim2.actor.cpp:1051+` → `moonpool-foundation/src/sim.rs`
- `FlowTransport.actor.cpp:1016-1125` → `moonpool-foundation/src/network/peer/`
- `Buggify.h:79-88` → `moonpool-foundation/src/buggify.rs`

---

### Orleans References

Located in `references/orleans/`:

#### Actor System Core
- **Grain.cs** - Base actor interface
- **IGrain.cs** - Actor interface contract
- **Catalog.cs** - Actor registry
- **ActivationData.cs** - Actor state and lifecycle
- **ActivationDirectory.cs** - Actor location/routing

#### Messaging
- **Message.cs** - Message structure
- **MessageCenter.cs** - Message routing (equivalent to MessageBus)
- **CallbackData.cs** - Request-response correlation
- **InsideRuntimeClient.cs** - Runtime messaging client

#### Scheduling
- **WorkItemGroup.cs** - Message queue per actor
- **ActivationTaskScheduler.cs** - Task scheduling
- **WorkQueue.cs** - Queue implementation

#### Lifecycle and Bootstrap
- **Silo.cs** - Actor runtime bootstrap, lifecycle orchestration
- **SystemTarget.cs** - Static infrastructure actors
- **GrainService.cs** - Partitioned infrastructure services
- **ISiloLifecycle.cs** - Lifecycle interface and stages
- **GrainCollectionOptions.cs** - Virtual actor configuration
- **DeactivationReason.cs** - Deactivation triggers
- **SiloLifecycle.cs** - Catalog lifecycle (equivalent to ActorCatalog state machine)

**Mapping to Phase 12**:
- Steps 1-2: `Catalog.cs`, `SiloLifecycle.cs`, `ActivationData.cs`
- Step 3: `Grain.cs`, `IGrain.cs`
- Step 4: `WorkItemGroup.cs`, `ActivationTaskScheduler.cs`
- Step 5: `Message.cs`, `MessageStatistics.cs`
- Step 6-7: `MessageCenter.cs`, `InsideRuntimeClient.cs`, `CallbackData.cs`
- Step 8: `ActivationData.cs`, `GrainCollectionOptions.cs`, `DeactivationReason.cs`

---

### TigerBeetle References

Located in `references/tigerbeetle/`:

#### Network Simulation
- **packet_simulator.zig** - Network fault configuration patterns (lines 12-488)
  - Packet loss models
  - Delay distributions
  - Fault injection strategies

**Relevant for**: Network configuration in foundation layer

---

## Development Guides

Crate-specific instructions for contributors.

### [../CLAUDE.md](../CLAUDE.md)
**Workspace-level development guide**
- Nix environment setup
- Phase completion criteria
- Git workflow and commit guidelines
- Cross-cutting constraints
- Test configuration
- Documentation index

---

### [../moonpool-foundation/CLAUDE.md](../moonpool-foundation/CLAUDE.md)
**Foundation crate development**
- Core constraints (no Send/Sync, no unwrap)
- Testing philosophy and chaos testing
- Architecture layers (simulation, networking, transport, testing)
- Reference documentation mapping
- Validation checklist
- When to read which FDB/TigerBeetle files

**Use this when**: Working on simulation core, transport layer, peer networking, or testing framework

---

### [../moonpool/CLAUDE.md](../moonpool/CLAUDE.md)
**Actor system development**
- Relationship to foundation
- Core concepts (ActorId, lifecycle)
- Architecture components (Catalog, Actor, Inbox, MessageBus)
- Testing strategy with state machines
- Orleans vocabulary mapping
- Reference documentation for each component
- Step-by-step validation checklist

**Use this when**: Working on actors, MessageBus, ActorCatalog, or virtual actor lifecycle

---

## Documentation Organization Principles

1. **Layered**: Specs → Analysis → References → Plans
2. **Cross-referenced**: Each doc links to related docs
3. **Phase-aligned**: Plans map to implementation phases
4. **Reference-driven**: Analysis explains production code patterns
5. **Actionable**: Development guides provide clear next steps

## Contributing to Documentation

When adding new documentation:
1. Update this index with description and cross-references
2. Link from related documents
3. Follow the established structure (specs, plans, analysis, references)
4. Include code references with line numbers where applicable
5. Map to relevant phase and crate (foundation vs moonpool)
