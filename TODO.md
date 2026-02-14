# Moonpool-Sim DX Redesign

Single PR, 10 phases, each committed independently. Designed for autonomous Claude Code execution.

## Execution Strategy

**Use a team when possible.** After Phase 6, Phases 7-10 can run in parallel:
- Teammate A: Phase 7 (exploration verification)
- Teammate B: Phases 8a + 9 (transport fork points + transport workloads)
- Teammate C: Phases 8b + 10 (actor fork points + actor workloads)

If a team is not already spawned, create one with `TeamCreate` and spawn up to 3 teammates for parallel phases. Phases 1-6 are sequential.

## Golden Rule

**When stuck, resolve to the most simple, incremental solution.** Do not overthink or over-engineer. If something doesn't compile, find the smallest change that fixes it. If a design is unclear, pick the simplest option that works. Iterate.

## Prerequisites

Before ANY work:
1. Read `CLAUDE.md` for project constraints (no unwrap, no tokio direct calls, async_trait(?Send), etc.)
2. Read this `TODO.md` for full context
3. **After any context compaction**, re-read both `CLAUDE.md` and `TODO.md` to restore context
4. **Track progress here**: Update each phase's `Status` field as work progresses (`NOT STARTED` → `IN PROGRESS` → `COMPLETE`). This file is the source of truth for resuming after context resets.

### Context documents

Read these for design context:

- **DX redesign spec** (full API design, before/after examples): https://gist.githubusercontent.com/PierreZ/0c8b2f5e356827d0d3734c5df702f2ee/raw/f4ccd0b1417379bc84a057a2c255a547d7483e8c/dapper-sauteeing-crystal.md
- **FDB workload patterns** (alphabets, invariants, reference models, 5 determinism rules): https://pierrezemb.fr/posts/writing-rust-fdb-workloads-that-find-bugs/

### Reference implementation

The fork-based exploration was prototyped in a separate project. Read these files for porting:

- **Maze workload** (4-lock gate cascade): `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/maze.rs`
- **Dungeon workload** (8-floor spatial exploration): `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/dungeon.rs`
- **Runner/orchestrator** (multiverse loop, reporting): `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/runner.rs`
- **Assertion + forking core** (all assertion types, EachBucket, coverage): `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/lib.rs`
- **Dungeon game engine**: `/home/pierrez/workspace/rust/Claude-fork-testing/dungeon/src/lib.rs`

## Rules

- **Each phase ends with a verification** that passes `nix develop --command cargo fmt && nix develop --command cargo clippy && nix develop --command cargo nextest run`
- **DO NOT commit.** Commits require GPG signing via Yubikey — the user must be physically present to sign. Prepare changes and verify they pass, then notify the user that a phase is ready for commit.
- **Never break the build**. If you need to remove code that other files depend on, update all dependents in the same phase. Only delete once dependents are updated.
- **Comment out, don't delete** old test code. Wrap commented-out code with `// TODO CLAUDE AI: <reason>` tags so it can be found later with grep. Use block comments `/* ... */` for multi-line sections.
- **Each phase updates this `TODO.md`** with a status line marking the phase complete.

---

## Phase 1: Comment out old tests, keep the build green

**Status**: COMPLETE

**Goal**: Comment out all simulation/chaos test files. Keep all library code compiling. The old macros, builder API, StateRegistry, and InvariantCheck remain for now (transport src depends on them).

### 1.1 Comment out test files

These are standalone test targets — commenting them out does not break library compilation:

```
COMMENT OUT moonpool-sim/tests/chaos/assertions.rs        — tag: "TODO CLAUDE AI: port to new assert_always!/assert_sometimes! macros"
COMMENT OUT moonpool-sim/tests/exploration/tests.rs        — tag: "TODO CLAUDE AI: port to new Workload trait API + add maze/dungeon exploration tests"
COMMENT OUT moonpool-transport/tests/simulation/workloads.rs   — tag: "TODO CLAUDE AI: port to new Workload trait API"
COMMENT OUT moonpool-transport/tests/simulation/invariants.rs  — tag: "TODO CLAUDE AI: port to new Invariant trait API"
COMMENT OUT moonpool-transport/tests/simulation/test_scenarios.rs — tag: "TODO CLAUDE AI: port to new builder API"
COMMENT OUT moonpool-transport/tests/e2e/workloads.rs      — tag: "TODO CLAUDE AI: port to new Workload trait API"
COMMENT OUT moonpool-transport/tests/e2e/invariants.rs     — tag: "TODO CLAUDE AI: port to new Invariant trait API"
```

Keep mod.rs stubs / directory structure if needed for remaining test files to compile.

### 1.2 Comment out builder inline tests

In `moonpool-sim/src/runner/builder.rs`: comment out the `#[cfg(test)] mod tests { ... }` block (lines 364-550) with tag `// TODO CLAUDE AI: port to new builder API`. The builder itself stays intact.

### 1.3 Verify

```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run
```

Update this `TODO.md`: mark Phase 1 complete.

Commit message (for user): `refactor(moonpool): comment out old simulation tests for DX redesign`

---

## Phase 2: EachBucket infrastructure in moonpool-explorer

**Status**: COMPLETE

**Goal**: Add `assert_sometimes_each!` backing infrastructure to moonpool-explorer. Isolated crate, no moonpool-sim changes.

### 2.1 Create `moonpool-explorer/src/each_buckets.rs` (~200 LOC)

Adapt the source code from Claude-fork-testing (`lib.rs` lines 989-1254) to moonpool-explorer's thread-local `Cell<*mut>` pattern (NOT global AtomicPtr). Replace `assertion_branch(MAX_ASSERTIONS + bucket_index)` with `crate::fork_loop::dispatch_branch(name, bucket_idx % crate::assertion_slots::MAX_ASSERTION_SLOTS)`.

**Port these items** (see Appendix A for full source):
- `EachBucket` struct (repr(C), Clone, Copy)
- `msg_hash()` (FNV-1a u32)
- `find_or_alloc_each_bucket()` (pointer passed as arg, read from thread-local at call site)
- `compute_each_bucket_index()`
- `pack_quality()` / `unpack_quality()`
- `assertion_sometimes_each()` (main backing function)
- `each_bucket_read_all()`

### 2.2 Wiring changes

**Update `moonpool-explorer/src/context.rs`**: Add `EACH_BUCKET_PTR: Cell<*mut u8>` thread-local (same pattern as `COVERAGE_BITMAP_PTR`).

**Update `moonpool-explorer/src/lib.rs`**:
- Add `pub mod each_buckets`
- In `init()`: allocate shared memory for EachBuckets (size = `8 + MAX_EACH_BUCKETS * size_of::<EachBucket>()`), store pointer in `EACH_BUCKET_PTR` thread-local
- In `cleanup()`: free the EachBucket shared memory
- Re-export: `assertion_sometimes_each`, `each_bucket_read_all`, `EachBucket`

### 2.3 Unit tests

Test `msg_hash`, `pack_quality`/`unpack_quality`, bucket allocation, quality watermark CAS.

### 2.4 Verify

Commit message: `feat(moonpool-explorer): add EachBucket infrastructure for assert_sometimes_each!`

---

## Phase 3: New assertion macros

**Status**: COMPLETE

**Goal**: Add new assertion macros alongside old ones. Old ones still needed by transport src until Phase 6.

### 3.1 Add macros to `moonpool-sim/src/chaos/assertions.rs`

```rust
/// Always-true assertion. Panics with seed info on failure.
#[macro_export]
macro_rules! assert_always {
    ($condition:expr, $message:expr) => {
        if !$condition {
            let seed = $crate::get_current_sim_seed();
            panic!("[ALWAYS FAILED] seed={} — {}", seed, $message);
        }
    };
}

/// Sometimes-true assertion. Records stats and triggers exploration fork on success.
#[macro_export]
macro_rules! assert_sometimes {
    ($condition:expr, $message:expr) => {
        let result = $condition;
        $crate::chaos::assertions::record_assertion($message, result);
        if result {
            $crate::chaos::assertions::on_sometimes_success($message);
        }
    };
}

/// Per-value bucketed sometimes assertion.
#[macro_export]
macro_rules! assert_sometimes_each {
    ($msg:expr, [ $(($name:expr, $val:expr)),+ $(,)? ]) => {
        $crate::chaos::assertions::on_sometimes_each($msg, &[ $(($name, $val as i64)),+ ], &[])
    };
    ($msg:expr, [ $(($name:expr, $val:expr)),+ $(,)? ], [ $(($qname:expr, $qval:expr)),+ $(,)? ]) => {
        $crate::chaos::assertions::on_sometimes_each(
            $msg,
            &[ $(($name, $val as i64)),+ ],
            &[ $(($qname, $qval as i64)),+ ],
        )
    };
}
```

### 3.2 Add backing function

```rust
pub fn on_sometimes_each(msg: &str, keys: &[(&str, i64)], quality: &[(&str, i64)]) {
    moonpool_explorer::assertion_sometimes_each(msg, keys, quality);
}
```

### 3.3 Verify

Commit message: `feat(moonpool-sim): add assert_always!, assert_sometimes!, assert_sometimes_each! macros`

---

## Phase 4: New simulation types

**Status**: COMPLETE

**Goal**: Add all new types. Everything additive — nothing breaks, no callers yet.

### 4.1 `StateHandle` — `moonpool-sim/src/chaos/state_handle.rs`

`Rc<RefCell<HashMap<String, Box<dyn Any>>>>` with:
- `publish<T: Any + 'static>(key, value)` — insert/replace
- `get<T: Any + Clone>(key) -> Option<T>` — downcast + clone
- `contains(key) -> bool`

### 4.2 `Invariant` trait — `moonpool-sim/src/chaos/invariant_trait.rs`

```rust
pub trait Invariant: 'static {
    fn name(&self) -> &str;
    fn check(&self, state: &StateHandle, sim_time_ms: u64);
}
```

Plus `invariant_fn(name, closure)` adapter returning `Box<dyn Invariant>`.

### 4.3 `SimContext` — `moonpool-sim/src/runner/context.rs`

Wraps `SimProviders` + `WorkloadTopology` + `StateHandle` + `CancellationToken`:
- `providers() -> &SimProviders` — pass to code generic over `P: Providers`
- `network()`, `time()`, `task()`, `random()`, `storage()` — convenience accessors via providers
- `my_ip()`, `peer()`, `peers()` — topology accessors
- `shutdown()` — workload shutdown signal
- `state()` — shared `StateHandle`

`SimProviders` already exists at `moonpool-sim/src/providers/sim_providers.rs` — reuse it.

Use `tokio_util::sync::CancellationToken` for shutdown (already a dependency).

### 4.4 `Workload` trait — `moonpool-sim/src/runner/workload.rs`

```rust
#[async_trait(?Send)]
pub trait Workload: 'static {
    fn name(&self) -> &str;
    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> { Ok(()) }
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> { Ok(()) }
}
```

Plus `FnWorkload` closure adapter and `workload_fn(name, closure)` helper.

### 4.5 `FaultInjector` + `FaultContext` — `moonpool-sim/src/runner/fault_injector.rs`

**Key design**: FaultInjector gets `FaultContext` (NOT `SimContext`). `FaultContext` wraps `SimWorld` to give real fault injection power.

`SimWorld` already has these methods (at `moonpool-sim/src/sim/world.rs`):
- `partition_pair(a, b)` (line 2175)
- `partition_send_from(a, b)` (line 2208)
- `partition_recv_to(a, b)` (line 2232)
- `restore_partition(a, b)` (line 2256)
- `is_partitioned(a, b)` (line 2268)
- `cut_connection(id, duration)` (line 1499)
- `close_connection(id)` (line 1736)

```rust
/// Context for fault injectors — gives access to SimWorld fault injection methods.
pub struct FaultContext {
    sim: SimWorld,
    all_ips: Vec<String>,
    random: SimRandomProvider,
    time: SimTimeProvider,
    chaos_shutdown: CancellationToken,  // fires at chaos→recovery boundary
}

impl FaultContext {
    pub fn partition(&self, a: &str, b: &str) { /* sim.partition_pair(...) */ }
    pub fn heal_partition(&self, a: &str, b: &str) { /* sim.restore_partition(...) */ }
    pub fn is_partitioned(&self, a: &str, b: &str) -> bool { /* sim.is_partitioned(...) */ }
    pub fn all_ips(&self) -> &[String] { ... }
    pub fn random(&self) -> &SimRandomProvider { ... }
    pub fn time(&self) -> &SimTimeProvider { ... }
    pub fn chaos_shutdown(&self) -> &CancellationToken { ... }
}

#[async_trait(?Send)]
pub trait FaultInjector: 'static {
    fn name(&self) -> &str;
    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()>;
}
```

### 4.6 `PhaseConfig` — TigerBeetle VOPR two-phase pattern

```rust
pub struct PhaseConfig {
    /// Duration of chaos phase (faults + workloads run concurrently).
    pub chaos_duration: Duration,
    /// Duration of recovery phase (faults stopped, workloads continue, system heals).
    pub recovery_duration: Duration,
}
```

### 4.7 Wire into mod.rs files

- `moonpool-sim/src/chaos/mod.rs`: add `state_handle`, `invariant_trait` modules + re-exports
- `moonpool-sim/src/runner/mod.rs`: add `context`, `workload`, `fault_injector` modules + re-exports
- `moonpool-sim/src/lib.rs`: add re-exports for new types

### 4.8 Verify

Commit message: `feat(moonpool-sim): add SimContext, Workload, FaultInjector, StateHandle, Invariant types`

---

## Phase 5: Rewrite builder + orchestrator

**Status**: COMPLETE

**Goal**: Replace builder and orchestrator internals with new lifecycle. All old-API callers are already commented out (Phase 1), so this is safe.

### 5.1 Rewrite builder (`moonpool-sim/src/runner/builder.rs`)

- Remove `WorkloadFn` type alias
- Remove `register_workload()`, `with_invariants()`
- New fields: `workloads: Vec<Box<dyn Workload>>`, `invariants: Vec<Box<dyn Invariant>>`, `fault_injectors: Vec<Box<dyn FaultInjector>>`, `phase_config: Option<PhaseConfig>`
- New methods: `workload(w: impl Workload)`, `workload_fn(name, closure)`, `invariant(i: impl Invariant)`, `invariant_fn(name, closure)`, `fault(f: impl FaultInjector)`, `phases(PhaseConfig)`, `random_network()` (rename of `use_random_config`), `until_all_sometimes_reached(n)`
- Keep: `set_iterations`, `set_debug_seeds`, `set_time_limit`, `enable_exploration`, `run()`

### 5.2 Rewrite orchestrator (`moonpool-sim/src/runner/orchestrator.rs`)

New lifecycle in `orchestrate_workloads()`:
1. Create shared `StateHandle`
2. Create `SimContext` per workload (unique IP, shared StateHandle + shutdown)
3. **Setup phase**: `workload.setup(ctx)` sequentially
4. **Chaos phase** (when `PhaseConfig` is set):
   - Create `FaultContext` with chaos-phase `CancellationToken`
   - `spawn_local` all `workload.run(ctx)` + `fault_injector.inject(fault_ctx)` concurrently
   - Event loop: `sim.step()` → `Invariant::check(state, time)` → yield
   - After `chaos_duration`: cancel `fault_ctx.chaos_shutdown()`, heal all partitions
   - **Recovery phase**: workloads keep running for `recovery_duration`
   - After recovery: cancel workload shutdown
5. **Without PhaseConfig**: first workload completion triggers shutdown (current behavior)
6. **Check phase**: after all runs + `sim.run_until_empty()`, call `workload.check(ctx)` sequentially
7. Explorer child exit (same as current)

Keep `IterationManager`, `MetricsCollector`, `DeadlockDetector` — they handle iteration logic.

### 5.3 Update topology

- Remove `state_registry` field from `WorkloadTopology` (`moonpool-sim/src/runner/topology.rs`)
- Update `TopologyFactory::create_topology()` — no StateRegistry param

### 5.4 Verify

Commit message: `feat(moonpool-sim): rewrite builder + orchestrator with Workload lifecycle and two-phase chaos/recovery`

---

## Phase 6: Port transport macros + clean up old code

**Status**: COMPLETE

**Goal**: Replace old macro calls in transport src, delete old types. Everything compiles.

### 6.1 Port macro calls (~6 files, ~33 calls)

Mechanical replacement:
- `always_assert!(name, cond, msg)` → `assert_always!(cond, msg)`
- `sometimes_assert!(name, cond, msg)` → `assert_sometimes!(cond, msg)`

Files: `rpc/net_transport.rs`, `peer/core.rs`, `rpc/net_notified_queue.rs`, `rpc/endpoint_map.rs`, `rpc/reply_promise.rs`, `rpc/reply_future.rs`.

### 6.2 Delete old code (now safe — no dependents remain)

- Delete `always_assert!` and `sometimes_assert!` macro definitions from assertions.rs
- Delete `moonpool-sim/src/chaos/state_registry.rs`
- Delete `moonpool-sim/src/chaos/invariants.rs`
- Update `moonpool-sim/src/chaos/mod.rs`: remove old modules/re-exports, add new ones
- Update `moonpool-sim/src/runner/mod.rs`: finalize re-exports
- Update `moonpool-sim/src/lib.rs`: swap re-exports
- Update `moonpool/src/lib.rs`: update facade re-exports

### 6.3 Verify

Commit message: `refactor(moonpool): port assertion macros, delete StateRegistry/InvariantCheck`

---

## Phase 7: Exploration verification workloads

**Status**: NOT STARTED

**Goal**: Port maze + dungeon from Claude-fork-testing as moonpool-sim integration tests. These prove fork-based exploration actually works end-to-end — finding paths through probability gates that random testing would take millions of iterations to discover.

**Can run in parallel with Phases 8-10 after Phase 6 completes.**

### 7.1 Port maze workload

**Source**: `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/maze.rs`

Port as `moonpool-sim/tests/exploration/maze.rs` using new `Workload` trait:
- 4 locks, each behind 5 nested probability gates (P=0.05)
- `assert_sometimes_each!("gate", [("lock", L), ("depth", D)])` at each gate
- Bug: all 4 locks open → workload returns error
- Without exploration: P = 0.05^20 ≈ impossible
- With exploration: finds all paths in seconds

### 7.2 Port dungeon workload

**Source**: `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/dungeon.rs`
**Game engine source**: `/home/pierrez/workspace/rust/Claude-fork-testing/dungeon/src/lib.rs`

Port as `moonpool-sim/tests/exploration/dungeon.rs`:
- 8-floor game with keys, monsters, rooms, health
- `assert_sometimes_each!("descended", [("to_floor", f)], [("health", hp)])` with quality watermarks
- Quality keys: health = better starting position for exploration
- Bug: reach treasure on floor 8

### 7.3 Re-port existing exploration tests

Port the commented-out tests from Phase 1 to new API:
- `test_fork_basic` — `workload_fn` + `assert_sometimes!`
- `test_depth_limit`, `test_energy_limit`
- `test_planted_bug` — 3-gate cascade
- `test_sometimes_each_triggers_fork` (NEW)

### 7.4 Verify

Commit message: `feat(moonpool-sim): port maze + dungeon exploration workloads, verify fork exploration works end-to-end`

---

## Phase 8: Fork points in transport + actor source

**Status**: NOT STARTED

**Goal**: Add NEW `assert_sometimes!` and `assert_sometimes_each!` assertions as exploration fork points. These go in **library source** (not tests) so every simulation run hits them. Phase 6 already ports ~33 existing assertions to new macro names — Phase 8 adds genuinely new ones that don't exist yet.

**Can run in parallel with Phase 7 after Phase 6 completes. Split into 8a (transport) and 8b (actors) for parallel teammates.**

### Assertion placement principles

1. **Depth tracking**: Use `assert_sometimes_each!` with a depth/count key when reaching deeper states requires "sequential luck" (multiple successive rare events). The explorer forks at each depth, letting children explore deeper levels independently. This gives an nth-root asymptotic speedup over naive random testing.

2. **Chaos pairing**: Place `assert_sometimes!` inside or after `buggify!` blocks. These mark chaos-injected faults with low effective probability (~0.5%). Forking here explores the error recovery pipeline that follows the injection.

3. **Rare event marking**: Use `assert_sometimes!` for events that require specific conditions (chaos injection, timing, multi-step sequences). Don't add for events that fire on every run — those are already covered by Phase 6's ported assertions and would waste fork budget.

4. **Lifecycle stages**: For the actor system (zero existing assertions), mark lifecycle boundaries that are progressively harder to reach. Reactivation after deactivation is the hardest and most valuable fork point.

### 8a: Transport fork points (`moonpool-transport/src/`)

All entries below are NEW assertions not present in the existing codebase.

#### `peer/core.rs` — Connection Reliability Depth

| Location | Type | What to assert | Rationale |
|----------|------|----------------|-----------|
| `establish_connection()`, after `failure_count += 1` (line ~919) | `assert_sometimes_each!` | `"backoff_depth", [("attempt", failure_count as i64)]` | Each successive backoff doubles the delay. Getting to attempt 3+ requires sustained connection failure across multiple retry cycles. The explorer forks at attempt N and children explore deeper attempts with different seeds — the core depth-tracking pattern. Without it, reaching high backoff depths requires an exponentially unlikely sequence of consecutive failures. |
| Inside `buggify_with_prob!(0.02)` block (line ~600) | `assert_sometimes!` | `true, "buggified_write_failure"` | Marks chaos-injected write failure (~0.5% effective probability = 25% activation × 2% firing). Fork explores the post-failure pipeline: reliable requeue, unreliable discard, connection teardown, backoff, reconnection, and message redelivery. Pairs with existing `peer_requeues_on_failure` and `peer_recovers_after_failures` to verify the full recovery sequence. |
| When `read` returns 0 bytes (EOF) in `connection_task` (line ~646) | `assert_sometimes!` | `true, "graceful_close_on_read"` | TCP half-close (FIN delivery) path. Chaos-dependent — requires `random_close` buggify or explicit peer close. Fork explores post-FIN behavior: pending send buffer handling, connection teardown, peer state cleanup. Validates the half-close fix (graceful vs abort close semantics). |
| `establish_connection()`, when `max_connection_failures` check triggers return (line ~849) | `assert_sometimes!` | `true, "max_failures_reached"` | Terminal failure — peer gives up reconnecting. Very rare: requires max consecutive failures without any intervening success. Tests that the system handles permanent peer loss gracefully (error propagation to callers, workload fallback). |
| `establish_connection()`, inside timeout/failure branch `Ok(Err(_)) \| Err(_)` (line ~915) | `assert_sometimes!` | `true, "connection_timed_out"` | Connection attempt timed out (distinct from connection refused). Tests timeout handling and its interaction with backoff delay calculation. Chaos-dependent — requires simulated network delays to exceed `connection_timeout`. |

#### `rpc/reply_promise.rs` — RPC Error Paths

| Location | Type | What to assert | Rationale |
|----------|------|----------------|-----------|
| `send()` serialization failure path, inside `Err(e)` match arm (line ~117) | `assert_sometimes!` | `true, "reply_serialization_failed"` | Success response fails to serialize, server sends error reply instead. Very rare — requires a type that serializes on the request path but fails on the response path. Tests the error fallback path in reply handling. |
| `send_error()` method body (line ~142) | `assert_sometimes!` | `true, "error_reply_sent"` | Server explicitly sends an error response (distinct from broken promise). Tests the application-level error path through the RPC system. Requires workload to exercise error cases. |

### 8b: Actor fork points (`moonpool/src/actors/`)

The actor system currently has **zero assertions**. All additions are new. Listed from easiest to hardest to reach — the hardest transitions are the most valuable fork points.

#### `host.rs` — Actor Lifecycle

| Location | Type | What to assert | Rationale |
|----------|------|----------------|-----------|
| After successful `on_activate()` (line ~496, after `actor = Some(new_actor)`) | `assert_sometimes!` | `true, "actor_activated"` | Coverage baseline. Verifies exploration reaches actor activation code. Fork cost is minimal (one fork per seed) and provides a baseline signal from the actor system. |
| After `on_activate()` returns `Err` (line ~484) | `assert_sometimes!` | `true, "on_activate_failed"` | Activation failure. Only fires when the actor's `on_activate` returns an error (injected by test workload or chaos). Tests that the system sends error response, skips dispatch, and continues processing. |
| After `on_deactivate()` in `DeactivateOnIdle` check (line ~543) | `assert_sometimes!` | `true, "deactivate_on_idle"` | Per-dispatch deactivation. Only fires for actors with `DeactivateOnIdle` hint. Tests the core Orleans grain lifecycle: activate → dispatch → deactivate → reactivate. |
| After `on_deactivate()` in idle timeout path (line ~455) | `assert_sometimes!` | `true, "deactivate_after_idle_timeout"` | Idle timeout deactivation. Requires no messages for the configured duration. Harder to reach than per-dispatch deactivation because it needs a period of silence for a specific identity. |
| When actor reactivates after prior deactivation (line ~475) | `assert_sometimes!` | `true, "actor_reactivated"` | **Hardest lifecycle state.** Requires: (1) actor activates, (2) deactivation triggers, (3) new message arrives for same identity. Each step is independently unlikely. Fork here ensures thorough testing of state restoration and lifecycle correctness. **Impl**: add `let mut was_previously_active = false;` local in `identity_processing_loop`, set `true` after first activation, assert on re-activation. |
| When mailbox closed detected (loop exit at `None` match, line ~469) | `assert_sometimes!` | `true, "identity_mailbox_closed"` | Shutdown path. Fires when the routing loop closes the identity mailbox during host shutdown. Fork explores shutdown → deactivate → cleanup with various in-flight message states. |

#### `router.rs` — Routing Decisions

| Location | Type | What to assert | Rationale |
|----------|------|----------------|-----------|
| When `placement.place()` is called in `resolve()` (line ~189) | `assert_sometimes!` | `true, "placement_invoked"` | New actor placement. Fires when the directory has no entry for the target actor. Tests placement strategy → directory registration path. |
| When `AlreadyRegistered` race is handled in `resolve()` (line ~195) | `assert_sometimes!` | `true, "directory_registration_race"` | **Rare**: requires two callers to place the same actor identity simultaneously. Tests race resolution correctness. Chaos-dependent (requires timing luck or concurrent workloads). |

#### `state.rs` — State Store

| Location | Type | What to assert | Rationale |
|----------|------|----------------|-----------|
| When `write_state` returns `ETagMismatch` (line ~159) | `assert_sometimes!` | `true, "etag_conflict"` | Optimistic concurrency conflict. Requires concurrent writes to the same actor state key with stale ETags. Central to persistent actor correctness under contention. |

#### `persistent_state.rs` — Persistent State Lifecycle

| Location | Type | What to assert | Rationale |
|----------|------|----------------|-----------|
| When `load()` finds existing state (line ~65, `Some(entry)` branch) | `assert_sometimes!` | `true, "persistent_state_loaded"` | Fires when state was previously persisted and is being loaded. Combined with `actor_reactivated`, verifies the full write → deactivate → reactivate → load lifecycle. |
| After successful `write_state()` (line ~128) | `assert_sometimes!` | `true, "persistent_state_written"` | State durability marker. Paired with `persistent_state_loaded` to verify round-trip correctness. |

### 8.1 Verify

Commit message: `feat(moonpool): add exploration fork points in transport and actor source`

---

## Phase 9: Transport workloads

**Status**: NOT STARTED

**Goal**: FDB-style alphabet workloads for transport. Patterns from the blog post: operation alphabets (Normal/Adversarial/Nemesis), reference model (BTreeMap), conservation laws.

**Can run in parallel with Phase 10 after Phase 8a completes.**

### 9.1 Operation alphabet — `moonpool-transport/tests/simulation/alphabet.rs`

```rust
enum TransportOp {
    // Normal (70%): SendReliable, SendUnreliable, SendRpc, SmallDelay
    // Adversarial (20%): SendEmptyPayload, SendMaxSizePayload, SendToUnknownEndpoint
    // Nemesis (10%): UnregisterEndpoint, ReregisterEndpoint, DropRpcPromise
}
```

### 9.2 Reference model — `moonpool-transport/tests/simulation/reference_model.rs`

BTreeMap-based (deterministic iteration per FDB rules):

```rust
struct TransportRefModel {
    reliable_sent: BTreeMap<u64, MessageRecord>,
    reliable_received: BTreeMap<u64, MessageRecord>,
    unreliable_sent: BTreeMap<u64, MessageRecord>,
    unreliable_received: BTreeMap<u64, MessageRecord>,
    rpc_requests_sent: BTreeMap<u64, MessageRecord>,
    rpc_responses_received: BTreeMap<u64, MessageRecord>,
    rpc_broken_promises: BTreeSet<u64>,
    rpc_timeouts: BTreeSet<u64>,
    duplicate_count: u64,
}
```

### 9.3 Invariants

| Invariant | Type | Rule |
|-----------|------|------|
| No phantom reliable | assert_always! | `received ⊆ sent` |
| No phantom unreliable | assert_always! | `received ⊆ sent` |
| Unreliable conservation | assert_always! | `|received| <= |sent|` |
| No duplicate reliable | assert_always! | dup_count == 0 |
| RPC single resolution | assert_always! | responses, broken, timeouts disjoint |
| RPC no phantoms | assert_always! | response_ids ⊆ request_ids |
| All reliable delivered | assert_sometimes! (check phase) | received == sent |
| Some unreliable dropped | assert_sometimes! | `|received| < |sent|` |
| RPC success path | assert_sometimes! | `|responses| > 0` |
| RPC broken promise path | assert_sometimes! | `|broken| > 0` |

### 9.4 Workload implementations — `moonpool-transport/tests/simulation/workloads.rs`

`LocalDeliveryWorkload` — single-node, alphabet-driven. Uses `Workload` trait with setup/run/check lifecycle.

`ServerWorkload` + `ClientWorkload` — multi-node RPC. Server: setup binds listener, run accepts+processes. Client: setup connects, run sends alphabet ops.

All workloads publish `TransportRefModel` to `ctx.state()` after each operation.

### 9.5 Test scenarios — `moonpool-transport/tests/simulation/test_scenarios.rs`

```rust
// Fast (fixed seeds)
test_local_delivery_happy_path
test_local_delivery_adversarial
test_multi_node_rpc_1x1

// Slow chaos (UntilAllSometimesReached)
slow_simulation_local_delivery
slow_simulation_multi_node_rpc
```

### 9.6 E2E workloads — `moonpool-transport/tests/e2e/`

Port/adapt existing e2e patterns (operations.rs `PeerOp` + `OpWeights` is already well-structured) to new Workload trait API.

### 9.7 Verify

Commit message: `feat(moonpool-transport): FDB-style alphabet workloads with reference model invariants`

---

## Phase 10: Actor workloads

**Status**: NOT STARTED

**Goal**: FDB-style alphabet workloads for the virtual actor system. Conservation laws, ETag concurrency, lifecycle verification.

**Can run in parallel with Phase 9 after Phase 8b completes.**

### Actor system overview (for context)

Key types in `moonpool/src/actors/`:
- **`ActorHandler`**: `on_activate()`, `dispatch(method, body)`, `on_deactivate()`, `deactivation_hint()`
- **`ActorHost`**: routing loop per actor type + per-identity processing tasks
- **`ActorRouter`**: resolves actor location via directory, sends request
- **`PersistentState<T>`**: ETag-guarded state — `load()`, `write_state()`, `clear_state()`
- **`DeactivationHint`**: `KeepAlive`, `DeactivateOnIdle`, `DeactivateAfterIdle(Duration)`
- **Don't call `stop().await` in sim workloads** — use `drop(host)` instead

Example: `moonpool/examples/banking.rs`

### 10.1 Operation alphabet — `moonpool/tests/simulation/alphabet.rs`

```rust
enum ActorOp {
    // Normal (60%): Deposit, Withdraw, GetBalance, Transfer
    // Adversarial (25%): SendToNonExistent, InvalidMethod, ConcurrentCallsSameActor, ZeroAmount
    // Nemesis (15%): DeactivateActor, CorruptStateStore, FloodSingleActor
}
```

### 10.2 Reference model — `moonpool/tests/simulation/reference_model.rs`

```rust
struct ActorRefModel {
    balances: BTreeMap<String, i64>,
    total_deposited: u64,
    total_withdrawn: u64,
    ops_per_actor: BTreeMap<String, u64>,
    activations: BTreeMap<String, u64>,
    deactivations: BTreeMap<String, u64>,
}
```

### 10.3 Invariants

| Invariant | Type | Rule |
|-----------|------|------|
| Balance never negative | assert_always! | `balance >= 0` for all actors |
| Conservation law | assert_always! | `sum(balances) == total_deposited - total_withdrawn` |
| Activate before dispatch | assert_always! | activation_count >= 1 when ops > 0 |
| Unknown method → error | assert_always! | invalid method → ActorError, not panic |
| Actor eventually activated | assert_sometimes! | activation_count > 0 |
| DeactivateOnIdle exercised | assert_sometimes! | deactivate→reactivate cycle observed |
| Insufficient funds rejected | assert_sometimes! | withdraw > balance fails gracefully |
| Transfer completes | assert_sometimes! | cross-actor transfer succeeds |

### 10.4 Workload implementations — `moonpool/tests/simulation/workloads.rs`

`BankingWorkload` — single-node, alphabet-driven:
- **setup**: Create `ActorHost` with `InMemoryStateStore`, register `BankAccountImpl`, create router
- **run**: Loop picking random `ActorOp`, execute via `BankAccountRef`, update `ActorRefModel` in `ctx.state()`
- **check**: Verify conservation law, verify final balances match reference model

### 10.5 Test scenarios — `moonpool/tests/simulation/test_scenarios.rs`

```rust
test_banking_happy_path
test_banking_adversarial
test_banking_nemesis
slow_simulation_banking
```

### 10.6 Verify

Commit message: `feat(moonpool): virtual actor simulation workloads with conservation law invariants`

---

## Existing code to reuse

- `SimProviders` at `moonpool-sim/src/providers/sim_providers.rs` — wraps all providers, use inside SimContext
- `SimWorld` partition/connection methods at `moonpool-sim/src/sim/world.rs:1499-2268` — use inside FaultContext
- `WorkloadTopology` at `moonpool-sim/src/runner/topology.rs` — keep, remove `state_registry` field
- `TopologyFactory::create_topology()` — adapt (no StateRegistry param)
- `IterationManager`, `MetricsCollector`, `DeadlockDetector` in orchestrator — keep for iteration logic
- `SimulationReport`, `ExplorationReport` in `runner/report.rs` — keep unchanged
- `AssertionStats`, `record_assertion`, `get_assertion_results` — keep for stats tracking
- `buggify!`, `buggify_with_prob!` — keep unchanged
- `moonpool-transport/tests/e2e/operations.rs` — `PeerOp` + `OpWeights` pattern, adapt for new API

## Deferred

- `assert_sometimes_greater_than!` (watermark forking) — see Appendix B
- `assert_sometimes_all!` (frontier forking) — see Appendix B
- Multi-core workers
- Coverage in `step()` (hash prev_event x curr_event)
- Buggify as branch points

## Verification

After each phase: `nix develop --command cargo fmt && nix develop --command cargo clippy && nix develop --command cargo nextest run`

---

## Appendix A: Source code to port for EachBuckets

Full source from Claude-fork-testing. See Phase 2 for adaptation instructions.

### Constants + EachBucket struct

```rust
pub const MAX_EACH_BUCKETS: usize = 256;
pub const MAX_EACH_KEYS: usize = 6;
const EACH_MSG_LEN: usize = 32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EachBucket {
    pub site_hash: u32,
    pub bucket_hash: u32,
    pub fork_triggered: u8,
    pub num_keys: u8,
    pub has_quality: u8,
    pub _pad: u8,
    pub pass_count: u32,
    pub best_score: i64,
    pub key_values: [i64; MAX_EACH_KEYS],
    pub msg: [u8; EACH_MSG_LEN],
}

impl EachBucket {
    pub fn msg_str(&self) -> &str {
        let len = self.msg.iter().position(|&b| b == 0).unwrap_or(EACH_MSG_LEN);
        std::str::from_utf8(&self.msg[..len]).unwrap_or("???")
    }
}
```

### msg_hash (FNV-1a)

```rust
fn msg_hash(msg: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in msg.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}
```

### find_or_alloc_each_bucket

**ADAPT**: Use thread-local `Cell<*mut u8>` pattern, pointer passed as argument.

```rust
fn find_or_alloc_each_bucket(
    ptr: *mut u8, site_hash: u32, bucket_hash: u32,
    keys: &[(&str, i64)], msg: &str, has_quality: u8,
) -> *mut EachBucket {
    unsafe {
        let next_atomic = &*(ptr as *const AtomicU32);
        let count = next_atomic.load(Ordering::Relaxed) as usize;
        let base = ptr.add(8) as *mut EachBucket;
        for i in 0..count.min(MAX_EACH_BUCKETS) {
            let bucket = base.add(i);
            if (*bucket).site_hash == site_hash && (*bucket).bucket_hash == bucket_hash {
                return bucket;
            }
        }
        let new_idx = next_atomic.fetch_add(1, Ordering::Relaxed) as usize;
        if new_idx >= MAX_EACH_BUCKETS {
            next_atomic.fetch_sub(1, Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        let bucket = base.add(new_idx);
        let mut msg_buf = [0u8; EACH_MSG_LEN];
        let n = msg.len().min(EACH_MSG_LEN - 1);
        msg_buf[..n].copy_from_slice(&msg.as_bytes()[..n]);
        let mut key_values = [0i64; MAX_EACH_KEYS];
        let num_keys = keys.len().min(MAX_EACH_KEYS);
        for (i, &(_, v)) in keys.iter().take(num_keys).enumerate() {
            key_values[i] = v;
        }
        std::ptr::write(bucket, EachBucket {
            site_hash, bucket_hash, fork_triggered: 0,
            num_keys: num_keys as u8, has_quality, _pad: 0,
            pass_count: 0, best_score: i64::MIN, key_values, msg: msg_buf,
        });
        bucket
    }
}
```

### assertion_sometimes_each (main backing function)

**ADAPT**: Read from `Cell` thread-local. Use `dispatch_branch(name, idx)` instead of `assertion_branch()`.

```rust
pub fn assertion_sometimes_each(msg: &str, keys: &[(&str, i64)], quality: &[(&str, i64)]) {
    let ptr = EACH_BUCKET_PTR.load(Ordering::Relaxed);  // ADAPT: Cell thread-local
    if ptr.is_null() { return; }
    let site_hash = msg_hash(msg);
    let mut bucket_hash = site_hash;
    for &(_, val) in keys {
        for b in val.to_le_bytes() {
            bucket_hash ^= b as u32;
            bucket_hash = bucket_hash.wrapping_mul(0x01000193);
        }
    }
    let has_quality = quality.len().min(4) as u8;
    let score = if has_quality > 0 { pack_quality(quality) } else { 0 };
    let bucket = find_or_alloc_each_bucket(ptr, site_hash, bucket_hash, keys, msg, has_quality);
    if bucket.is_null() { return; }
    unsafe {
        let count_atomic = &*((&(*bucket).pass_count) as *const u32 as *const AtomicU32);
        count_atomic.fetch_add(1, Ordering::Relaxed);
        let ft = &*((&(*bucket).fork_triggered) as *const u8 as *const AtomicU8);
        let first_discovery = ft.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed).is_ok();
        if first_discovery {
            if has_quality > 0 {
                let bs = &*((&(*bucket).best_score) as *const i64 as *const AtomicI64);
                bs.store(score, Ordering::Relaxed);
            }
            let idx = compute_each_bucket_index(ptr, bucket);
            crate::fork_loop::dispatch_branch(msg, idx % crate::assertion_slots::MAX_ASSERTION_SLOTS);
        } else if has_quality > 0 {
            let bs = &*((&(*bucket).best_score) as *const i64 as *const AtomicI64);
            let mut current = bs.load(Ordering::Relaxed);
            loop {
                if score <= current { break; }
                match bs.compare_exchange_weak(current, score, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => {
                        let idx = compute_each_bucket_index(ptr, bucket);
                        crate::fork_loop::dispatch_branch(msg, idx % crate::assertion_slots::MAX_ASSERTION_SLOTS);
                        break;
                    }
                    Err(actual) => current = actual,
                }
            }
        }
    }
}
```

### each_bucket_read_all + helpers

```rust
fn compute_each_bucket_index(base_ptr: *mut u8, bucket: *const EachBucket) -> usize {
    if base_ptr.is_null() { return 0; }
    let buckets_base = unsafe { base_ptr.add(8) } as usize;
    let offset = (bucket as usize).saturating_sub(buckets_base);
    offset / std::mem::size_of::<EachBucket>()
}

fn pack_quality(quality: &[(&str, i64)]) -> i64 {
    let mut packed: i64 = 0;
    for (i, &(_, v)) in quality.iter().take(4).enumerate() {
        packed |= ((v as u16) as i64) << ((3 - i) * 16);
    }
    packed
}

pub fn unpack_quality(packed: i64, n: u8) -> Vec<i64> {
    (0..n as usize).map(|i| ((packed >> ((3 - i) * 16)) as u16) as i64).collect()
}

pub fn each_bucket_read_all() -> Vec<EachBucket> {
    let ptr = EACH_BUCKET_PTR.load(Ordering::Relaxed);  // ADAPT: Cell thread-local
    if ptr.is_null() { return Vec::new(); }
    unsafe {
        let count = (*(ptr as *const u32)) as usize;
        let count = count.min(MAX_EACH_BUCKETS);
        let base = ptr.add(8) as *const EachBucket;
        (0..count).map(|i| std::ptr::read(base.add(i))).collect()
    }
}
```

---

## Appendix B: Deferred assertion types

Source from Claude-fork-testing. Can be added to moonpool-explorer later.

### assert_sometimes_greater_than! (numeric watermark forking)

```rust
pub fn assertion_numeric(cmp: AssertCmp, left: i64, right: i64, msg: &str) {
    // ... slot lookup, watermark CAS loop, fork on fork_watermark improvement ...
}

#[macro_export]
macro_rules! assert_sometimes_greater_than {
    ($left:expr, $right:expr, $msg:expr) => {
        $crate::assertion_numeric(AssertCmp::Gt, $left as i64, $right as i64, $msg)
    };
}
```

### assert_sometimes_all! (frontier forking)

```rust
pub fn assertion_sometimes_all(msg: &str, conditions: &[(&str, bool)]) {
    // ... slot lookup, count true booleans, CAS loop on frontier, fork on advance ...
}

#[macro_export]
macro_rules! assert_sometimes_all {
    ($msg:expr, [ $(($name:expr, $val:expr)),+ $(,)? ]) => {
        $crate::assertion_sometimes_all($msg, &[ $(($name, $val)),+ ])
    };
}
```

---

## Appendix C: Assertion usage patterns from dungeon.rs

Reference patterns from `/home/pierrez/workspace/rust/Claude-fork-testing/maze-explorer/src/dungeon.rs`. Adapt for transport and actor workloads.

### Pattern 1: Per-value bucketed fork points with quality watermarks

```rust
if game.on_key_tile() && !game.has_key() {
    assert_sometimes_each!(
        "on key tile",
        [("floor", level as i64)],           // identity: one bucket per floor
        [("health", hp_bucket as i64)]        // quality: re-fork if health improves
    );
}
```

### Pattern 2: Outcome-driven fork points

```rust
match outcome {
    StepOutcome::Won => { assert_sometimes!(true, "treasure found"); }
    StepOutcome::Descended => {
        assert_sometimes_each!("descended", [("to_floor", new_level as i64)], [("health", hp as i64)]);
    }
    _ => {}
}
```

### Pattern 3: Gate amplification

```rust
// Fork BEFORE the probability gate — children resume with fresh seeds
assert_sometimes_each!("stairs with key", [("floor", level as i64)], [("health", hp as i64)]);
if random_bool(RARE_EVENT_P) {
    // rare path now much more likely to be explored
}
```

### Key rules

- Place `assert_sometimes_each!` **before** probability gates (not after)
- Use identity keys for bucketing: `[("actor_id", id), ("retry_count", n)]`
- Use quality keys for watermarks: `[("queue_depth", depth)]`
- `assert_sometimes!` for boolean events: "this happened at least once"
