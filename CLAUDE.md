# Moonpool: Deterministic Simulation Framework

## Environment & Commands
**Nix shell required**: `nix develop --command <cargo-command>`

**Phase completion**: All must pass:
- `nix develop --command cargo fmt`
- `nix develop --command cargo clippy`
- `nix develop --command cargo nextest run`

**Validation**: Each phase/subphase must validate through:
- Full compilation (code + tests)
- All tests passing
- No test timeouts or hangs

## Core Constraints
- Single-core execution (no Send/Sync)
- No `unwrap()` - use `Result<T, E>` with `?`
- Document all public items
- Networking: `#[async_trait(?Send)]`
- Use traits, not concrete types
- KISS principle
- Use LocalRuntime, not LocalSet (spawn_local without Builder)
- **No direct tokio calls**: Use Provider traits (`TimeProvider::sleep()`, not `tokio::time::sleep()`)
  - Forbidden: `tokio::time::sleep()`, `tokio::time::timeout()`, `tokio::spawn()`
  - Required: `time.sleep()`, `time.timeout()`, `task_provider.spawn_task()`

## Testing Philosophy
**Goal**: 100% sometimes assertion coverage via chaos testing
**Target**: 100% success rate - no deadlocks/hangs acceptable

**Failing seeds**: Debug with `SimulationBuilder::set_seed(failing_seed)` → fix root cause → verify → re-enable chaos
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

## References
**Read first**: `docs/analysis/flow.md` (before any `actor.cpp` code)
**Architecture**: `docs/analysis/fdb-network.md`

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