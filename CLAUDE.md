# Moonpool: Deterministic Simulation Framework

## Environment & Commands
**Nix shell required**: `nix develop --command <cargo-command>`

**Validation**: All must pass before completing work:
- `nix develop --command cargo fmt`
- `nix develop --command cargo clippy`
- `nix develop --command cargo nextest run --profile fast`

**IMPORTANT**: NEVER run `cargo nextest run` without `--profile fast`. The default profile runs ALL tests including slow simulation tests that saturate CPU for minutes. Always use a profile.

**Test profiles**: Configured in `.config/nextest.toml`, aliases in `.cargo/config.toml`
- `cargo test-fast` — fast tests only (unit + SimWorld) — **use this by default**
- `cargo test-sim` — slow simulation tests only (chaos + exploration) — CPU intensive, runs 1 at a time
- `cargo nextest run --profile fast` — same as `cargo test-fast`
- `cargo nextest run --profile sim` — same as `cargo test-sim`

**Naming convention**: Slow tests use `slow_simulation_` prefix (e.g. `slow_simulation_maze`). Fast tests use `test_` prefix.

**Debug testing**:
- Default: `UntilAllSometimesReached(1000)` for comprehensive chaos testing
- Debug faulty seeds: `FixedCount(1)` with specific seed and ERROR log level

## Commit Messages
Follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):

Format: `<type>(<crate>): <description>`

Types: `fix` (bugfix), `feat` (new feature), `build`, `chore`, `ci`, `docs`, `style`, `refactor`, `perf`, `test`

## Core Constraints
- Single-core execution (no Send/Sync)
- No `unwrap()` - use `Result<T, E>` with `?`
- Document all public items
- Networking: `#[async_trait(?Send)]`
- Use traits, not concrete types
- KISS principle
- **No LocalSet usage**: Use `tokio::runtime::Builder::new_current_thread().build_local()` only
- **No direct tokio calls**: Use Provider traits
  - Forbidden: `tokio::time::sleep()`, `tokio::time::timeout()`, `tokio::spawn()`
  - Required: `time.sleep()`, `time.timeout()`, `task_provider.spawn_task()`

## Crate Architecture
```
moonpool/                    - Facade crate + virtual actors, re-exports everything
moonpool-core/               - Provider traits (Time, Task, Network, Random, Storage) and core types
moonpool-sim/                - Simulation runtime, chaos testing, buggify, assertions
moonpool-transport/          - Peer connections, wire format, FlowTransport, RPC
moonpool-transport-derive/   - Proc-macro: #[service] and #[actor_impl]
moonpool-explorer/           - Fork-based multiverse exploration, coverage, energy budgets
```

## Testing Philosophy
**Goal**: 100% sometimes assertion coverage via chaos testing + comprehensive invariant validation
**Target**: 100% success rate - no deadlocks/hangs acceptable

**Multi-seed testing**: Default `UntilAllSometimesReached(1000)` runs until all sometimes_assert! statements have triggered
**Failing seeds**: Debug with `SimulationBuilder::set_seed(failing_seed)` → fix root cause → verify → re-enable chaos
**Infrastructure events**: Tests terminate early when only ConnectionRestore events remain
**Invariant checking**: Cross-workload properties validated after every simulation event
**Goal**: Find bugs, not regression testing

## Storage Testing Patterns
**Key difference from network**: Storage operations return `Poll::Pending` and require simulation stepping. Network operations buffer and return `Poll::Ready` immediately.

**The Step Loop Pattern** (required for storage tests):
```rust
let handle = tokio::task::spawn_local(async move {
    let mut file = provider.open("test.txt", OpenOptions::create_write()).await?;
    file.write_all(b"hello").await?;
    file.sync_all().await
});

while !handle.is_finished() {
    while sim.pending_event_count() > 0 {
        sim.step();
    }
    tokio::task::yield_now().await;
}
handle.await.unwrap().unwrap();
```

**Key functions**:
- `sim.step()` - Process one simulation event
- `sim.pending_event_count()` - Check for pending events
- `sim.storage_provider()` - Create storage provider for simulation
- `tokio::task::yield_now()` - Yield to spawned tasks
- `handle.is_finished()` - Check task completion

**Fault coverage** (TigerBeetle + FDB patterns):
- Read/write corruption, crash/torn writes
- Misdirected reads/writes, phantom writes
- Sync failures, uninitialized reads
- IOPS/bandwidth timing simulation

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

## Invariant System
**When to use invariants**: Cross-actor properties, global system constraints, deterministic bug detection
**When to use assertions**: Per-actor validation (`always_assert!` in actor code)
**Performance**: Invariants run after every simulation event - design accordingly
**Architecture**: Actors expose state via JSON → StateRegistry → InvariantCheck functions → panic on violation
