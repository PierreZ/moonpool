# Moonpool Foundation: Deterministic Simulation Framework

✅ **COMPLETE - HOBBY-GRADE**: This framework is fully implemented and comprehensively tested. Suitable for simulation testing of distributed systems but remains hobby-grade and not recommended for production use.

## Overview

Moonpool Foundation is a deterministic simulation framework inspired by FoundationDB's simulation testing approach. It provides infrastructure for developing and testing distributed systems through controlled, reproducible simulations, particularly designed for testing the moonpool actor system.

The framework features a sophisticated **Sans I/O transport layer** with request-response semantics, enabling you to write distributed system logic once and test it with simulated networking for predictable debugging, then deploy with real networking - all using identical application code.

**Note**: This is a hobby-grade project suitable for simulation testing and experimentation, not for production systems.

## Key Features

### Core Simulation Infrastructure

- **Deterministic Event Processing**: Logical time engine with priority-based event ordering
- **Reproducible Testing**: Same seed = identical behavior for bug reproduction
- **Provider Pattern**: Seamlessly swap between simulated and real implementations
- **Single-Core Design**: Simplified async without thread-safety complexity
- **Thread-Local RNG**: Deterministic random number generation with seed-based control

### Sans I/O Transport Layer

Built with clean async/await patterns inspired by FoundationDB but modernized for Rust:

- **Self-Driving Futures**: `request()` and `try_next_message()` internally manage transport state
- **Type-Safe Messaging**: Request-response with automatic correlation IDs
- **Binary Wire Format**: Length-prefixed serialization with partial read support
- **Multi-Connection Support**: Servers handle multiple concurrent connections automatically
- **Connection Management**: Automatic reconnection with exponential backoff

### Comprehensive Chaos Testing

- **Buggify System**: FoundationDB-style deterministic fault injection
- **Sometimes Assertions**: Statistical validation with coverage tracking
- **Always Assertions**: Invariants that must never be violated
- **Cross-Workload Invariants**: JSON-based state registry for validating global properties
- **Multi-Seed Testing**: Comprehensive edge case exploration with `UntilAllSometimesReached`
- **Hostile Infrastructure**: Network conditions 10x worse than production

### Network Simulation

- **TCP Connection Management**: Point-to-point connections with full lifecycle
- **Configurable Chaos**: Packet loss, delays, and partitions
- **Multi-Topology Testing**: Support for 1x1 through 10x10 client-server topologies
- **7 Bug Detectors**: Message conservation, per-peer accounting, correlation validation

## Quick Start

### Basic Usage

```rust
use moonpool_foundation::prelude::*;

// Define your message types
#[derive(Serialize, Deserialize)]
struct PingMessage { sequence: u64 }

#[derive(Serialize, Deserialize)]
struct PongMessage { sequence: u64 }

// Client-side: Send request, get response
async fn client(transport: ClientTransport<...>) -> Result<()> {
    let response: PongMessage = transport
        .request::<PingMessage, PongMessage>(PingMessage { sequence: 1 })
        .await?;

    println!("Got response: {}", response.sequence);
    Ok(())
}

// Server-side: Event-driven message handling
async fn server(transport: ServerTransport<...>) -> Result<()> {
    while let Some((request, responder)) = transport.try_next_message().await? {
        let ping: PingMessage = request;
        let pong = PongMessage { sequence: ping.sequence };
        responder.send(pong).await?;
    }
    Ok(())
}
```

### Simulation Testing

```rust
use moonpool_foundation::simulation::*;

async fn ping_pong_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    // Create server with simulated providers
    let server = ServerBuilder::new()
        .listen_addr("127.0.0.1:5000")
        .with_providers(network.clone(), time.clone(), task_provider.clone())
        .build()
        .await?;

    // Create client
    let client = ClientBuilder::new()
        .connect_addr("127.0.0.1:5000")
        .with_providers(network, time, task_provider)
        .build()
        .await?;

    // Run workload with buggify and assertions
    // ... your test logic here

    Ok(SimulationMetrics::default())
}

// Run simulation with multiple seeds
#[tokio::test]
async fn test_ping_pong_chaos() {
    let result = SimulationBuilder::new()
        .with_workload(ping_pong_workload)
        .with_strategy(TestStrategy::UntilAllSometimesReached(1000))
        .with_network_config(NetworkConfig::hostile())
        .run()
        .await
        .unwrap();

    assert!(result.all_sometimes_reached());
}
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Application Code                         │
├─────────────────────────────────────────────────────────────┤
│                Transport Layer (Sans I/O)                   │
│  ┌─────────────────┐  ┌─────────────────┐                  │
│  │ ClientTransport │  │ ServerTransport │                  │
│  │ • request()     │  │ • try_next_msg()│                  │
│  │ • Self-driving  │  │ • Event-driven  │                  │
│  └─────────────────┘  └─────────────────┘                  │
├─────────────────────────────────────────────────────────────┤
│            Provider Traits (Network, Time, Task)            │
├─────────────────────────────────────────────────────────────┤
│  Simulation Mode           │      Production Mode           │
│  • Deterministic           │      • Real Tokio networking   │
│  • Logical time            │      • Wall-clock time         │
│  • Chaos testing           │      • System resources        │
└─────────────────────────────────────────────────────────────┘
```

## Testing Philosophy

**Goal**: Find bugs before production through hostile infrastructure simulation

- **Deterministic**: Same seed = identical behavior
- **Chaos by default**: 10x worse than production conditions
- **Comprehensive coverage**: All error paths tested via `sometimes_assert!`
- **Invariant validation**: Cross-workload properties checked after every event
- **100% success rate**: No deadlocks or hangs acceptable

### Assertions System

```rust
// Always assertions - must never fail
always_assert!(condition, "invariant violated");

// Sometimes assertions - should occur under normal conditions
sometimes_assert!(condition, "expected behavior");
```

### Buggify System

Strategic fault injection for testing error paths:

```rust
if buggify!(0.25) {
    // Inject 250ms delay 25% of the time (when buggify enabled)
    time.sleep(Duration::from_millis(250)).await;
}
```

### Invariant Validation

Global properties validated after every simulation event:

```rust
// Register state for invariant checking
state_registry.register("component_name", json!({
    "sent": sent_count,
    "received": received_count,
    "in_transit": in_transit_count
}));

// Invariant: Message conservation
always_assert!(
    sent == received + in_transit,
    "messages must be conserved"
);
```

## Implementation Status

### ✅ Complete - Hobby-Grade

All core features are implemented and tested, but the project remains hobby-grade:

- **Core Simulation** (SimWorld, event processing, logical time)
- **Provider Pattern** (Network, Time, Task, Random providers)
- **Sans I/O Transport** (ClientTransport, ServerTransport, envelope system)
- **Connection Management** (TCP lifecycle, automatic reconnection)
- **Buggify System** (Deterministic fault injection)
- **Assertions** (Always/sometimes with coverage tracking)
- **Invariants** (Cross-workload validation system)
- **Multi-Topology Testing** (1x1 through 10x10 topologies)
- **Network Simulation** (Peer connections, fault injection)
- **403 Tests Passing** (404 total, 1 skipped) - Comprehensive coverage with chaos testing

**Limitations**: Hobby-grade quality, suitable for simulation testing but not production systems.

### Provider Implementations

| Provider | Simulation | Production |
|----------|-----------|------------|
| **Network** | `SimNetworkProvider` | `TokioNetworkProvider` |
| **Time** | `SimTimeProvider` | `TokioTimeProvider` |
| **Task** | N/A | `TokioTaskProvider` |
| **Random** | `SimRandomProvider` | System RNG |

## API Highlights

### Transport Layer

**Client**:
```rust
let response: Response = client_transport
    .request::<Request, Response>(request)
    .await?;
```

**Server**:
```rust
while let Some((request, responder)) = server_transport.try_next_message().await? {
    responder.send(response).await?;
}
```

### Provider Pattern

```rust
// Always use provider traits, never direct tokio calls
time.sleep(duration).await;              // ✅ Correct
// tokio::time::sleep(duration).await;   // ❌ Wrong

task_provider.spawn_task(async move { });  // ✅ Correct
// tokio::spawn(async move { });           // ❌ Wrong
```

## Testing

Run the comprehensive test suite:

```bash
# All tests (101 tests)
nix develop --command cargo nextest run -p moonpool-foundation

# Specific test
nix develop --command cargo nextest run -p moonpool-foundation test_name

# With logging
RUST_LOG=debug nix develop --command cargo nextest run -p moonpool-foundation
```

Test configuration in `.config/nextest.toml`:
- Default slow-timeout: 5 seconds
- Slow simulation tests: 10 minute timeout
- TokioRunner tests: 60 second timeout

## Examples

See the test suite for extensive examples:
- `tests/autonomous_ping_pong.rs` - Comprehensive autonomous chaos testing with 7 bug detectors and multi-topology validation
- `tests/transport_e2e.rs` - End-to-end transport layer validation
- `tests/simulation_integration.rs` - Multi-topology testing

## Design Principles

1. **API Familiarity**: APIs match what developers expect from tokio objects
2. **Provider Pattern**: Seamless simulation/production switching
3. **Sans I/O Design**: Protocol logic separated from I/O operations
4. **FoundationDB Inspiration**: Proven simulation testing approach
5. **Single-Core Simplicity**: No Send/Sync requirements
6. **State Machines**: Explicit state machines for complex lifecycle management

## Performance Characteristics

- **Single-core execution**: No thread synchronization overhead
- **Logical time**: Simulation advances instantly (no real delays)
- **Deterministic**: Perfect reproducibility for debugging
- **Fast iteration**: 1000+ seeds tested in reasonable time

## Related Crates

- [moonpool](../moonpool): Virtual actor system built on this foundation (early alpha)

## Documentation

- **API Docs**: Run `cargo doc --open -p moonpool-foundation`
- **Main Specification**: See `../docs/specs/moonpool-foundation-spec.md`
- **Transport Layer Spec**: See `../docs/specs/transport-layer-spec.md`
- **Testing Framework**: See `../docs/specs/testing-framework-spec.md`
- **Development Guide**: See `CLAUDE.md` for contributor instructions
- **FoundationDB Analysis**: See `../docs/analysis/foundationdb/flow.md`

## Comparison to FoundationDB

| Feature | FoundationDB | Moonpool Foundation |
|---------|--------------|---------------------|
| **Language** | C++ with Flow | Rust with async/await |
| **Simulation** | Custom actor runtime | Tokio LocalRuntime |
| **Networking** | FlowTransport | Sans I/O with tokio |
| **Testing** | Buggify + asserts | Buggify + asserts + invariants |
| **API Style** | Actors with promises | Async/await futures |

We preserve FoundationDB's core testing philosophy while providing modern Rust ergonomics.

## License

Apache 2.0
