# Moonpool

<p align="center">
  <img src="images/logo.png" alt="Moonpool Logo" />
</p>

A Rust workspace containing two distinct projects:
- **moonpool-foundation**: A standalone deterministic simulation framework (✅ COMPLETE)
- **moonpool**: A virtual actor system similar to Orleans (❌ NOT BUILT YET)

> **Note:** This is currently a hobby-grade project. Only **moonpool-foundation** is implemented and production-ready. The actor system is in the planning phase.

## Project Components

### moonpool-foundation (✅ COMPLETE - Standalone Framework)

A production-ready **deterministic simulation framework** inspired by FoundationDB's simulation testing approach. This is a **standalone library** that can be used independently to test any distributed system.

**Key Features:**
- Sans I/O transport layer with request-response semantics
- Comprehensive chaos testing infrastructure (Buggify, sometimes assertions)
- Provider pattern for seamless simulation/production switching
- Complete TCP connection management with automatic reconnection
- Cross-workload invariant validation system

**Status:** Fully implemented, all tests passing, ready for use.

### moonpool (❌ NOT BUILT YET - Actor System)

A planned **virtual actor system** similar to Microsoft Orleans, featuring location transparency and automatic lifecycle management.

**Planned Features:**
- Location-transparent actor addressing
- Automatic activation, deactivation, and migration
- MessageBus and ActorCatalog runtime infrastructure
- Built on top of moonpool-foundation

**Status:** Phase 12+ planning only - no implementation yet. Design documents and Orleans analysis available in `docs/`.

## Overview

**The current implementation is moonpool-foundation**, a comprehensive framework for developing and testing distributed systems through deterministic simulation. The framework features a sophisticated Sans I/O transport layer with request-response semantics, enabling you to write distributed system logic once and test it with simulated networking for predictable debugging, then deploy with real networking - all using identical application code.

## Features

### Core Simulation Framework
- **Deterministic Simulation** - Reproducible testing with controlled time and event ordering
- **Provider Pattern** - Seamlessly swap between simulated and real implementations (Network, Time, Task, Random)
- **Event-Driven Architecture** - Logical time advancement with priority-based event processing
- **Single-Core Design** - Simplified async without thread-safety complexity

### Sans I/O Transport Layer
- **Request-Response Semantics** - Type-safe envelope-based messaging with correlation IDs
- **Self-Driving Futures** - Clean async/await API without manual transport ticking
- **Connection Management** - Automatic reconnection, multi-connection servers, connection multiplexing
- **Binary Protocol** - Efficient length-prefixed wire format with automatic serialization

### Comprehensive Chaos Testing
- **Buggify System** - FoundationDB-style deterministic fault injection at strategic points
- **Sometimes Assertions** - Statistical validation with 100% coverage goals through chaos testing
- **Multi-Seed Testing** - Comprehensive edge case exploration across multiple deterministic runs
- **Hostile Infrastructure** - Network conditions 10x worse than production to expose bugs early
- **Cross-Workload Invariant Checking** - JSON-based system for validating global distributed system properties across multiple actors with 7 comprehensive bug detectors

## Architecture

The framework follows a layered architecture with clear separation of concerns:

```
┌─────────────────────────────────────────────────────────────┐
│                    Application Code                         │
├─────────────────────────────────────────────────────────────┤
│                Transport Layer (Sans I/O)                  │
│  ┌─────────────────┐  ┌─────────────────┐                 │
│  │ ClientTransport │  │ ServerTransport │                 │
│  │ • request()     │  │ • try_next_msg()│                 │
│  │ • Self-driving  │  │ • Event-driven  │                 │
│  └─────────────────┘  └─────────────────┘                 │
├─────────────────────────────────────────────────────────────┤
│            Provider Traits (Network, Time, Task)           │
├─────────────────────────────────────────────────────────────┤
│  Simulation Mode           │      Production Mode           │
│  • Deterministic           │      • Real Tokio networking   │
│  • Logical time            │      • Wall-clock time         │
│  • Chaos testing           │      • System resources        │
└─────────────────────────────────────────────────────────────┘
```

## Current Status

### moonpool-foundation: ✅ COMPLETE (Phases 1-11)
**Standalone deterministic simulation framework - production ready:**
- ✅ **Phase 11: Sans I/O Transport Layer** - Complete request-response semantics with envelope system
- ✅ **Provider Pattern** - Full abstraction layer (Network, Time, Task, Random providers)
- ✅ **Buggify Chaos Testing** - Deterministic fault injection with strategic placement
- ✅ **Sometimes Assertions** - Statistical validation system with coverage tracking
- ✅ **Multi-Seed Testing** - Comprehensive edge case exploration infrastructure
- ✅ **Connection Management** - Robust TCP layer with automatic reconnection and multiplexing
- ✅ **Cross-Workload Invariant System** - JSON-based state registry with global property validation
- ✅ **Multi-Topology Testing** - Support for 1x1 through 10x10 client-server topologies with comprehensive bug detection
- ✅ **Per-Peer Message Tracking** - Detailed accounting for message routing and load distribution validation

### moonpool: ❌ NOT IMPLEMENTED (Phase 12+)
**Virtual actor system - planning phase only:**
- Phase 12+ design documents and Orleans reference analysis available
- No code implementation yet
- Will build on moonpool-foundation when ready

## Getting Started

### Development Environment

```bash
# Enter development environment (Nix required)
nix develop

# Run all validation checks (required for each phase completion)
nix develop --command cargo fmt
nix develop --command cargo clippy  
nix develop --command cargo nextest run

# Build the project
nix develop --command cargo build
```

### Test Execution

Tests are configured with specific timeouts in `.config/nextest.toml`:
- Default tests: 1 second timeout
- Tests with "slow_simulation" in name: 4 minute timeout (for multi-topology chaos testing)

### Usage Examples

The transport layer provides clean async APIs for distributed system development:

**Client Example (simplified):**
```rust
// Type-safe request-response with automatic correlation
let response: PongMessage = client_transport
    .request::<PingMessage, PongMessage>(ping_msg)
    .await?;
```

**Server Example (simplified):**  
```rust
// Event-driven message handling across multiple connections
while let Some((request, responder)) = server_transport.try_next_message().await? {
    let response = handle_request(request).await;
    responder.send(response).await?;
}
```

## Testing Philosophy

**Goal: 100% Sometimes Assertion Coverage + Comprehensive Invariant Validation**

The framework uses "sometimes assertions" for statistical validation under chaos conditions:
- `always_assert!` - Invariants that must never be violated
- `sometimes_assert!` - Behaviors that should occur under normal conditions
- **Cross-workload invariants** - Global properties validated after every simulation event
- Target: Every sometimes assertion should trigger across multiple test seeds
- **7 Bug Detectors**: Message conservation, per-peer accounting, in-transit tracking, and correlation validation
- Comprehensive chaos testing exposes edge cases and validates error handling

**Multi-Seed Testing:**
- Each test runs with multiple deterministic seeds
- Same seed = identical behavior for bug reproduction
- Statistical analysis across all seeds for comprehensive coverage

## Project Structure

- **moonpool-foundation/** - ✅ **COMPLETE**: Standalone deterministic simulation framework (FDB-inspired)
- **moonpool/** - ❌ **NOT BUILT**: Virtual actor system (Orleans-like, planning only)
- **docs/** - Comprehensive documentation ([INDEX.md](docs/INDEX.md))
  - specs/ - Technical specifications (foundation layer complete)
  - plans/ - Phase implementation roadmaps (Phases 1-11 complete, 12+ planning)
  - analysis/ - Reference architecture analysis (FDB, Orleans)
  - references/ - Source code from FoundationDB, Orleans, TigerBeetle

## Documentation

**Start here**: [Main Specification](docs/specs/moonpool-foundation-spec.md) for framework overview

**Key specs**:
- [Transport Layer](docs/specs/transport-layer-spec.md) - Sans I/O architecture
- [Testing Framework](docs/specs/testing-framework-spec.md) - Chaos testing
- [Simulation Core](docs/specs/simulation-core-spec.md) - Core infrastructure
- [Peer Networking](docs/specs/peer-networking-spec.md) - TCP management

**Complete index**: See [docs/INDEX.md](docs/INDEX.md) for all documentation

## License

Apache 2.0