# Moonpool: Deterministic Simulation Framework

## Environment & Commands
**Nix shell required**: `nix develop --command <cargo-command>`

**Phase completion**: All must pass:
- `nix develop --command cargo fmt`
- `nix develop --command cargo clippy`
- `nix develop --command cargo nextest run`

**Test timeouts**: Configured in `.config/nextest.toml` (1s default, 4m for tests with "slow_simulation" in name)

**Validation**: Each phase/subphase must validate through:
- Full compilation (code + tests)
- All tests passing
- No test timeouts or hangs

**Debug testing**: 
- Default: `UntilAllSometimesReached(1000)` for comprehensive chaos testing
- Debug faulty seeds: `FixedCount(1)` with specific seed and ERROR log level

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

## Transport Layer (Phase 11 - COMPLETED)
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

**Developer Experience**: FoundationDB-inspired patterns with 70% code reduction in actors
- Single `tokio::select!` patterns replace complex manual loops
- Clean async/await without manual `tick()` calls
- Result-based error handling from message streams

## References
**Read first**: `docs/analysis/flow.md` (before any `actor.cpp` code)
**Architecture**: `docs/analysis/fdb-network.md`

**Available files in docs/references**:
- foundationdb/: Buggify.h, FlowTransport.actor.cpp, FlowTransport.h, Net2.actor.cpp, Net2Packet.cpp, Net2Packet.h, Ping.actor.cpp, sim2.actor.cpp
- tigerbeetle/: packet_simulator.zig

**IMPORTANT**: Always read FoundationDB's implementation first before making changes
- When working in simulation: always check Net2
- When working around network stuff like peer and Transport: always check FlowTransport

**Code mapping**:
- Peer → FlowTransport.h:147-191, FlowTransport.actor.cpp:1016-1125
- SimWorld → sim2.actor.cpp:1051+
- Config → tigerbeetle/packet_simulator.zig:12-488
- Connection → FlowTransport.actor.cpp:760-900
- Backoff → FlowTransport.actor.cpp:892-897
- Reliability → Net2Packet.h:30-111
- Ping → Ping.actor.cpp:29-38,147-169
- Queuing → Net2Packet.h:43-91
- Chaos → foundationdb/Buggify.h:79-88

**Focus**: TCP-level simulation (connection faults) not packet-level

## Current Architecture Status
**Phase 11 Complete**: Sans I/O transport layer with comprehensive testing
- **Multi-topology support**: 1x1, 1x2, 1x10, 2x2, 10x10 client-server configurations
- **JSON-based invariant system**: Cross-workload state registry with global property validation
- **7 comprehensive bug detectors**: Message conservation, per-peer accounting, in-transit tracking
- **Per-peer message tracking**: Detailed accounting for routing bugs and load distribution
- Strategic sometimes_assert placement for comprehensive chaos coverage
- Infrastructure event detection prevents infinite simulation loops
- RandomProvider trait for deterministic back-pressure generation
- Event-driven server architecture supporting multiple concurrent connections

## Invariant System
**When to use invariants**: Cross-actor properties, global system constraints, deterministic bug detection
**When to use assertions**: Per-actor validation (`always_assert!` in actor code)
**Performance**: Invariants run after every simulation event - design accordingly
**Architecture**: Actors expose state via JSON → StateRegistry → InvariantCheck functions → panic on violation