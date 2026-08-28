# Moonpool: Deterministic Simulation Framework

## Environment & Commands
**Nix shell required**: `nix develop --command <cargo-command>`

**Remote environments** (`CLAUDE_CODE_REMOTE=true`, e.g. Claude Code on the
web): nix is not preinstalled. Install it via the Ubuntu package — the
DeterminateSystems installer's CDN is blocked from these sandboxes:

```
sudo apt-get install -y nix-bin
mkdir -p ~/.config/nix && echo 'experimental-features = nix-command flakes' > ~/.config/nix/nix.conf
```

(`nix-bin` ships nix 2.18, which doesn't enable flakes by default — the
nix.conf line turns them on so `nix develop` works against this repo's
flake.) Without nix, `cargo nextest`, the cargo toolchain pinned by the
flake, and the simulation binaries are unavailable, and local results will
not match CI.

**Validation**: All must pass before completing work:
- `nix develop --command cargo fmt`
- `nix develop --command cargo clippy`
- `nix develop --command cargo nextest run`

**Clippy pedantic is enabled**. Never silence a clippy warning with
`#[allow(clippy::...)]` — fix the underlying issue (refactor the
function, split the type, rename the variable, etc.). The allow
attribute is not an acceptable resolution.

**Simulation binaries**: Managed via xtask:
- `cargo xtask sim list` — list all simulation binaries
- `cargo xtask sim run maze` — run binaries matching filter
- `cargo xtask sim run-all` — run all simulation binaries

**Debug testing**:
- Default: `UntilCoverageStable { plateau_seeds: 10, max_iterations: 1000 }` — runs adaptively until every `assert_sometimes!`/`assert_reachable!` has fired AND code coverage plateaus (real sancov edges under `cargo xtask sim run`; assertion-coverage fallback otherwise)
- Debug faulty seeds: `FixedCount(1)` with specific seed and ERROR log level

## Commit Messages
Follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):

Format: `<type>(<crate>): <description>`

Types: `fix` (bugfix), `feat` (new feature), `build`, `chore`, `ci`, `docs`, `style`, `refactor`, `perf`, `test`

## Core Constraints
- Single-thread execution via the moonpool deterministic executor
  (`moonpool_sim::executor::Executor`, seeded-random task scheduling), but traits
  are Send-bounded so customer code can use `Arc<RwLock<…>>`, `DashMap`, and
  `Arc<AtomicBool>` naturally. There is NO tokio runtime inside a simulation:
  a bare `tokio::spawn` panics at runtime — spawn via `ctx.task().spawn_task()`
  or `moonpool_sim::executor::spawn()`
- No `unwrap()` - use `Result<T, E>` with `?`; for `RwLock` poison, use
  `.expect("RwLock poisoned: prior task panicked")` (poisoning means a prior panic)
- Document all public items
- Provider traits use native AFIT (`async fn` in trait) with `Send + Sync + 'static`
  supertraits; trait declarations use `-> impl Future<…> + Send` to propagate Send
- Dyn-stored traits (`Process`, `Workload`, `FaultInjector`)
  use `#[async_trait]` (no `?Send`) with `Send + Sync + 'static` supertraits
- Use traits, not concrete types
- KISS principle
- **No LocalSet, no `spawn_local`, no `build_local`**: the executor runs on a
  single OS thread, but futures must be `Send + 'static`
- **No direct tokio calls**: Use Provider traits
  - Forbidden: `tokio::time::sleep()`, `tokio::time::timeout()`, `tokio::spawn()`,
    `tokio::select!` (entropy branch offsets — use `moonpool_sim::select!`)
  - Required: `time.sleep()`, `time.timeout()`, `task_provider.spawn_task()`

## Crate Architecture
```
crates/
├── moonpool/              - Facade crate; features: sim/tokio/hyper.
├── moonpool-core/         - Provider traits (Time, Task, Network, Random, Storage) + core types.
│                            Granular tokio features: tokio-task/-time/-net/-fs/-random
│                            (umbrella `tokio-providers`). wasm-clean with all off.
├── moonpool-assertions/   - Antithesis-style assertion accounting (pure std, ZERO deps, wasm-able).
│                            Heap table by default; explorer overlays MAP_SHARED + a discovery hook.
├── moonpool-buggify/      - Standalone buggify!/buggify_with_prob! macros + state (pure std, ZERO
│                            deps, wasm-able). Inert by default; moonpool-sim installs its seeded
│                            RNG per run and re-exports the macros. buggify_knob! stays in sim.
├── moonpool-sim/          - Simulation runtime, chaos testing, buggify, assertions wiring.
│                            feature `exploration` (default ON) gates moonpool-explorer; without it
│                            the sim compiles to wasm32-unknown-unknown.
├── moonpool-hyper/        - hyper 1.x integration: runtime adapters (HyperExecutor/HyperTimer),
│                            HyperIo, TowerToHyperService, ReconnectingChannel, H2Server. Generic
│                            over `P: Providers`; features `client`/`server` (both default on).
│                            Facade exposes it as `moonpool::hyper` behind feature `hyper`.
├── moonpool-explorer/     - Frontier exploration controller (libc/fork/mmap; never wasm).
│                            Recipes + bounded worker pool; depends on moonpool-assertions;
│                            optional dep of moonpool-sim.
├── moonpool-sim-examples/ - Runnable simulation workloads discovered by xtask.
├── moonpool-wasm-demo/    - Browser demo embedded in the book.
└── xtask/                 - Cargo xtask automation (simulation runner)
```

**Simulation internals** are split by ownership:
- `SimWorld` coordinates lifecycle and a global `Scheduler<Event>`. The scheduler
  alone owns monotonic logical time, same-time FIFO sequence allocation, and
  cancellation.
- `NetworkSimulation` owns network topology, connections, faults, pending
  operations, results, and network wakers. Bind, connect, and accept are real
  delayed operations.
- `StorageEngine` owns persistent files, independent open handles, disk configs
  and degradation episodes, exact pending/completed operations, faults, and
  storage wakers.
- Components return ordered scheduling/cancellation effects and `WakeBatch`es to
  the coordinator. Do not move component state or wakers back into `SimInner`,
  and never invoke a waker while holding the world lock.

**Dispatch**: providers stay **static-generic** (traits aren't object-safe: Clone supertrait,
generic `random<T>`, RPIT futures, associated types; sim is test-only so no runtime selection).
Where generics hurt, erase at an application boundary with a narrowly scoped trait.

**Production builds** keep sim/explorer out entirely:
`moonpool = { default-features = false, features = ["tokio"] }`.

**Portability checks** (must also pass when touching crate structure / features):
- `nix develop --command cargo clippy -p moonpool-sim --no-default-features --all-targets -- -D warnings`
- `nix develop --command cargo check --target wasm32-unknown-unknown -p moonpool-sim --no-default-features`

## Testing Philosophy
**Goal**: 100% sometimes assertion coverage via chaos testing + comprehensive invariant validation
**Target**: 100% success rate - no deadlocks/hangs acceptable

**Multi-seed testing**: Default `UntilCoverageStable` runs adaptively until all assert_sometimes! statements have triggered and code coverage plateaus (`.until_coverage_stable(plateau_seeds, max_iterations)` to tune)
**Failing seeds**: Debug with `SimulationBuilder::set_seed(failing_seed)` → fix root cause → verify → re-enable chaos
**Infrastructure events**: Tests terminate early when only partition-expiry events remain
**Invariant checking**: Cross-workload properties validated after every simulation step
**Goal**: Find bugs, not regression testing
**NEVER remove assertions that catch bugs** — if an assertion fails, fix the underlying bug. Assertions exist to find real issues; deleting a failing assertion hides the bug.
**When an assertion catches a bug**: Stop, enter plan mode, and enable deep thinking. Read relevant reference code, trace the full data flow, and understand the root cause before attempting a fix. Do not rush.

## Storage Testing Patterns
Storage read, write, sync, and set-length operations return `Poll::Pending` and
complete through scheduled `StorageEvent`s. Network bind, connect, and accept
are delayed too. Established stream I/O is buffer-driven and may complete on the
current poll when data or capacity is already available.

`SimulationBuilder::run()` drives the scheduler and executor for process and
workload tests. Low-level provider tests must drive the future and `SimWorld`
together on Moonpool's executor:

```rust
async fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    futures::pin_mut!(future);
    futures::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(output) => Poll::Ready(output),
        Poll::Pending if sim.has_pending_events() => {
            sim.step();
            cx.waker().wake_by_ref();
            Poll::Pending
        }
        Poll::Pending => Poll::Pending,
    })
    .await
}

let mut executor = moonpool_sim::executor::Executor::new(seed);
executor.block_on(async move {
    let provider = sim.storage_provider(ip);
    drive(&mut sim, async move {
        let mut file = provider.open("test.txt", OpenOptions::create_write()).await?;
        file.write_all(b"hello").await?;
        file.sync_all().await
    })
    .await
})?;
```

**Key functions**:
- `sim.step()` - Process one simulation event
- `sim.has_pending_events()` - Check whether the scheduler can make progress
- `sim.storage_provider()` - Create storage provider for simulation
- `executor::until_stalled()` - Let all tasks woken by a step run in orchestrator loops

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

## Compatibility
- Library is not in external use — prefer replacing code over maintaining backward compatibility
- No deprecation needed — delete old APIs when replacing them

## Documentation
**Keep the book up to date**: When changing public APIs, adding features, or modifying behavior in any moonpool crate, update the corresponding chapters in `book/src/`. Build with `nix develop --command mdbook build book/` to verify.

## API Design Principles
**Keep APIs familiar**: APIs should match what developers expect from tokio objects
- Maintain async patterns where developers expect them
- Use standard tokio types and conventions
- Avoid surprises in API behavior vs real networking

## References
**Read first**: `docs/analysis/foundationdb/layer-1-flow-runtime.md` (before any `actor.cpp` code)
**Available files in docs/references**:
- foundationdb/: Buggify.h, FlowTransport.actor.cpp, FlowTransport.h, Net2.actor.cpp, Net2Packet.cpp, Net2Packet.h, Ping.actor.cpp, sim2.actor.cpp
- tigerbeetle/: packet_simulator.zig

**IMPORTANT**: Always read FoundationDB's implementation first before making simulation changes.
- For simulated networking and connection behavior, check Net2 and `sim2.actor.cpp`.

**Code mapping**:
- SimWorld → sim2.actor.cpp:1051+
- Config → tigerbeetle/packet_simulator.zig:12-488
- Reliability → Net2Packet.h:30-111
- Ping → Ping.actor.cpp:29-38,147-169
- Queuing → Net2Packet.h:43-91
- Chaos → foundationdb/Buggify.h:79-88
- Process lifecycle → sim2.actor.cpp (reboot/kill mechanics)

**Focus**: TCP-level simulation (connection faults) not packet-level

## Process / Workload Separation
**Process** — system under test. Runs on server node. Factory creates fresh instance per boot.
**Workload** — test driver. Survives reboots. Drives requests and validates correctness.

**Builder API**:
- `.processes(count, factory)` — register server processes (IPs: `10.0.1.{1..N}`)
- `.tags(&[("key", &["val1", "val2"])])` — round-robin tag distribution
- `.enable_chaos([Chaos::Attrition { config, mode }])` — built-in chaos reboots (requires `.chaos_duration()`)
- `.enable_chaos([Chaos::Network(mode) | Chaos::Storage(mode)])` — network/storage faults per seed (`ChaosMode::Random` all-on, `ChaosMode::Swarm` per-seed subset)
- `.fault_factory(|| Box::new(injector))` — fresh custom `FaultInjector` per root/explored timeline (exploration-compatible; `FaultContext` has split `crash`/`restart` primitives + workload `state()`)
- `.swarm_operations()` — per-seed swarm of each workload's operation alphabet

**Reboot lifecycle**: Graceful (signal token → grace period → force kill → restart) vs Crash (immediate abort → restart)
**Key types**: `Process`, `RebootKind`, `Attrition`, `ProcessTags`, `TagRegistry`
**FDB reference**: Process ≈ `fdbd`, Workload ≈ `tester.actor.cpp`

## Invariant System
**When to use invariants**: Cross-process properties, global system constraints, deterministic bug detection
**When to use assertions**: Per-process validation (`assert_always!` in process code)
**Performance**: Invariants run after every simulation step - design accordingly (cursor-based `since`, not `snapshot`)

**Architecture**: Correctness facts are plain `tracing` events — the same instrumentation production observability consumes. `SimulationLayer` captures them into a timeline; the orchestrator runs registered invariants after each `sim.step()`.

```rust
// Emit (in process / workload code) — plain production-style tracing
tracing::info!(target: "raft", term, leader = %my_ip, "leader_elected");

// Observe (in the invariant) — query by event name, extract fields by key
fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
    for e in q.since("leader_elected", &self.cursor) {
        let term = e.u64("term");
        let leader = e.str("leader");
        ...
    }
}
```

- **Capture rule**: INFO+ level, non-empty constant message (= the event name), emitted inside a process/workload task. No special fields, no derives.
- **Source attribution**: the orchestrator wraps each process/workload task in `info_span!("process"/"workload", ip = %ip)`; the layer resolves `TraceEvent::source` from the nearest enclosing span. Events outside actor spans are dropped.
- **Field tips**: use `%` for strings (`?` on a `String` keeps Debug quotes); bytes go hex-encoded into a string field.
- **Sim faults**: engine records `SimFaultEvent`s internally (`SimWorld::take_faults()`); the runner merges them into the timeline under `SIM_FAULT_EVENT_NAME` (`"sim_fault"`) with a `kind` field and `source = "sim"`.
- **Sim time** is stamped by the layer from its internal clock (the orchestrator pushes `obs.set_sim_time_ms(...)` after each `sim.step()`). Do NOT include a `time_ms` field on the emit.
- **Invariants** run from the runner loop (not tracing dispatch); use the assertion macros, not raw `panic!`, and treat `observe(...)` as read-only.
- **Production**: any tracing subscriber (fmt, OpenTelemetry) sees the same emissions as structured events; cross-validation in sim uses the very same traces.
- **Canonical example**: `crates/moonpool-sim/tests/leader_election.rs` (split-brain detection).
