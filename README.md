# Moonpool

<p align="center">
  <img src="images/logo.png" alt="Moonpool Logo" />
</p>

A Rust workspace containing two distinct projects:
- **moonpool-foundation**: A standalone deterministic simulation framework (✅ COMPLETE - HOBBY-GRADE)
- **moonpool**: A virtual actor system similar to Orleans (⚠️ EARLY ALPHA - HOBBY-GRADE)

> **Note:** This is a hobby-grade project. **moonpool-foundation** is complete and can be used for simulation testing, but remains hobby-grade. **moonpool** is in early alpha with working core features but should be considered experimental.

## Project Components

### moonpool-foundation (✅ COMPLETE - Hobby-Grade Framework)

A **deterministic simulation framework** inspired by FoundationDB's simulation testing approach. This is a **standalone library** that can be used independently to test distributed systems, particularly for moonpool's actor system simulation.

**Key Features:**
- Sans I/O transport layer with request-response semantics
- Comprehensive chaos testing infrastructure (Buggify, sometimes assertions)
- Provider pattern for seamless simulation/production switching
- Complete TCP connection management with automatic reconnection
- Cross-workload invariant validation system

**Status:** Fully implemented, all tests passing. Suitable for simulation testing but remains hobby-grade.

### moonpool (⚠️ EARLY ALPHA - Actor System)

An **experimental virtual actor system** similar to Microsoft Orleans, featuring location transparency and automatic lifecycle management. Core features are functional but the project is in early alpha and should be considered experimental.

**Implemented Features:**
- ✅ Location-transparent actor addressing (ActorRef with call/send)
- ✅ Automatic activation on first message (ActorCatalog with factories)
- ✅ MessageBus with request-response correlation
- ✅ Directory-based actor placement across nodes
- ✅ Pluggable state persistence (Storage trait with JSON serialization)
- ✅ Multi-node networking via moonpool-foundation transport
- ✅ Actor lifecycle management (activation, deactivation, message processing)

**Working Examples:**
- `hello_actor` - Virtual actor demonstration with multi-node deployment
- `bank_account` - State persistence with DeactivateOnIdle policy

**Status:** Early alpha - core functionality works but lacks production hardening, comprehensive testing, and monitoring. Built on moonpool-foundation for deterministic simulation.

## Overview

This workspace provides **two complementary tools** for building distributed systems:

1. **moonpool-foundation** (✅ Complete - Hobby-Grade): A comprehensive framework for developing and testing distributed systems through deterministic simulation. Features a sophisticated Sans I/O transport layer with request-response semantics, enabling you to write distributed system logic once and test it with simulated networking for predictable debugging, then deploy with real networking - all using identical application code. Suitable for simulation testing but remains hobby-grade.

2. **moonpool** (⚠️ Early Alpha - Hobby-Grade): An experimental actor system built on moonpool-foundation, providing Orleans-style virtual actors with location transparency, automatic activation, and state persistence. Core features work but the project is in early development.

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

### moonpool-foundation: ✅ COMPLETE - HOBBY-GRADE
**Standalone deterministic simulation framework - suitable for simulation testing:**
- ✅ **Sans I/O Transport Layer** - Complete request-response semantics with envelope system
- ✅ **Provider Pattern** - Full abstraction layer (Network, Time, Task, Random providers)
- ✅ **Buggify Chaos Testing** - Deterministic fault injection with strategic placement
- ✅ **Sometimes Assertions** - Statistical validation system with coverage tracking
- ✅ **Multi-Seed Testing** - Comprehensive edge case exploration infrastructure
- ✅ **Connection Management** - Robust TCP layer with automatic reconnection and multiplexing
- ✅ **Cross-Workload Invariant System** - JSON-based state registry with global property validation
- ✅ **Multi-Topology Testing** - Support for 1x1 through 10x10 client-server topologies with comprehensive bug detection
- ✅ **Per-Peer Message Tracking** - Detailed accounting for message routing and load distribution validation
- ✅ **403 tests passing** (404 total, 1 skipped) - Comprehensive test coverage with deterministic chaos testing

### moonpool: ⚠️ EARLY ALPHA - FUNCTIONAL BUT EXPERIMENTAL
**Virtual actor system - core features working, needs production hardening:**
- ✅ **ActorCatalog** - Local actor registry with double-check locking and auto-activation
- ✅ **MessageBus** - Full routing with correlation IDs, local/remote dispatch, request-response
- ✅ **ActorContext** - Per-actor message processing loop with lifecycle management
- ✅ **ActorRuntime** - Multi-node runtime with builder pattern and graceful startup
- ✅ **Directory** - Location tracking and placement decisions (Random/Local/LeastLoaded)
- ✅ **Storage** - Pluggable state persistence with JSON serialization
- ✅ **Network Integration** - Built on moonpool-foundation transport layer
- ✅ **76 tests passing** - Unit tests for core components
- ✅ **2 working examples** - hello_actor and bank_account demonstrating multi-node scenarios
- ⚠️ **Lacks**: Production monitoring, comprehensive integration tests, time-based idle timeout, replication

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
- Default slow-timeout: 5 seconds
- Tests with "slow_simulation" in name: 10 minute timeout (for multi-topology chaos testing)
- Tests with "tokio_runner" in name: 60 second timeout (for production networking validation)

### Usage Examples

**moonpool-foundation Transport Layer:**

```rust
// Client: Type-safe request-response with automatic correlation
let response: PongMessage = client_transport
    .request::<PingMessage, PongMessage>(ping_msg)
    .await?;

// Server: Event-driven message handling across multiple connections
while let Some((request, responder)) = server_transport.try_next_message().await? {
    let response = handle_request(request).await;
    responder.send(response).await?;
}
```

**moonpool Actor System:**

```rust
use moonpool::prelude::*;

// Create actor runtime
let runtime = ActorRuntime::builder()
    .namespace("example")
    .listen_addr("127.0.0.1:5000")
    .build()
    .await?;

// Register actor type with factory
runtime.register_actor::<BankAccountActor, _>(BankAccountActorFactory)?;

// Get actor reference (location-transparent)
let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice")?;

// Call actor methods (auto-activation, state persistence)
let balance = alice.deposit(100).await?;
```

See `moonpool/examples/` for complete working examples.

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

- **moonpool-foundation/** - ✅ **COMPLETE - HOBBY-GRADE**: Standalone deterministic simulation framework (FDB-inspired)
- **moonpool/** - ⚠️ **EARLY ALPHA - HOBBY-GRADE**: Virtual actor system (Orleans-like, functional but experimental)
- **docs/** - Comprehensive documentation ([INDEX.md](docs/INDEX.md))
  - specs/ - Technical specifications
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