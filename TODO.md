# Moonpool Development Plan

> **Rule**: Mark each commit as ~~done~~ in this file when completed. Each commit = one git commit.

---

## [x] Exploration Infrastructure (`moonpool-explorer`)

Fork-based multiverse exploration for deterministic simulation testing. Splits timelines at assertion discovery points to find rare bugs. Inspired by FoundationDB's simulation and Antithesis.

### [x] Core exploration crate
- `moonpool-explorer/` — standalone leaf crate, only depends on `libc`
- `fork()` + `MAP_SHARED` memory for cross-process state
- Sequential fork tree: parent waits on child, merges coverage, loops
- `CoverageBitmap` + `ExploredMap` (8192-bit bitmaps) for new-path detection
- `SharedStats`, `SharedRecipe` for bug replay (`"count@seed -> count@seed"` format)
- RNG hooks (`fn()->u64` get_count, `fn(u64)` reseed) — zero moonpool knowledge

### [x] Antithesis assertion suite
- 15 assertion macros: `assert_always!`, `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!`, numeric variants (`_greater_than!`, etc.), `assert_sometimes_all!`, `assert_sometimes_each!`
- Rich `AssertionSlot` (112 bytes) in shared memory, counter-based layout
- Non-panicking always-assertions (Antithesis principle: assertions never crash)
- `validate_assertion_contracts()` for post-run verification
- `EachBucket` infrastructure for per-value bucketed assertions (256 buckets)

### [x] Adaptive forking + energy budgets
- 3-level energy: global → per-mark → reallocation pool
- Coverage-yield-driven batch forking: barren marks return energy
- `AdaptiveConfig { batch_size, min_timelines, max_timelines, per_mark_energy }`

### [x] Multi-core parallel exploration
- `Parallelism` enum: `MaxCores`, `HalfCores`, `Cores(n)`, `MaxCoresMinus(n)`
- Sliding window of concurrent fork children capped at core count
- Bitmap pool: one coverage bitmap slot per active child
- `waitpid(-1)` reaps whichever child finishes first

### [x] Coverage-preserving multi-seed exploration
- `prepare_next_seed()`: selective reset preserving explored map + watermarks
- Warm start: `warm_min_timelines` for barren marks on subsequent seeds
- Pass/fail counts accumulate across seeds (avoids false "was never reached")
- `UntilConverged` iteration control: stop when all sometimes reached + no new coverage
- `convergence_timeout` + `is_success()` for exit code reporting

### [x] Sancov integration
- LLVM `inline-8bit-counters` via `__sanitizer_cov_8bit_counters_init`
- `SANCOV_CRATES` build infrastructure for selective instrumentation
- Edge coverage wired into fork loop as exploration signal
- `sancov_edges_covered` / `sancov_edges_total` in stats and reporting
- `moonpool-sim-examples` crate extracted for focused sancov coverage

### [x] Rich reporting
- `SimulationReport` with `assertion_details`, `bucket_summaries`, per-seed metrics
- `ExplorationReport` with timelines, fork points, bugs, coverage bits, convergence
- Colored terminal display with sections for assertions, buckets, violations, exploration

### [x] Bug fixes
- Duplicate assertion slots from parallel fork children (TOCTOU in `find_or_alloc_slot`, tombstone dedup)
- False "was never reached" from `prepare_next_seed()` zeroing counts
- Coverage bitmap not set for `assert_sometimes_each!` (broke adaptive forking)
- Assertion violations decoupled from workload errors

---

## SpaceSim: Incremental Virtual Actor Simulation

## Context

Moonpool has virtual actors (`ActorHandler`, `PersistentState`, `MoonpoolNode`, directory, placement, membership) but **no simulation workload tests them under chaos**. The existing banking sim is single-node only, uses no `Process` trait, no attrition, no network chaos. Multi-node actors in simulation have **never been tested**. This plan builds a space-economy themed workload ("spacesim") that incrementally tests every layer: single-actor lifecycle, multi-node placement, cross-actor RPC, network faults, and process reboots.

## Files overview

**Delete** (commit 1):
- `moonpool/src/simulations/banking/` (entire directory: `mod.rs`, `workloads.rs`, `invariants.rs`, `operations.rs`)
- `moonpool/src/bin/sim/banking_chaos.rs`

**Create** (across commits):
- `moonpool/src/simulations/spacesim/mod.rs`
- `moonpool/src/simulations/spacesim/actors.rs` — StationActor, ShipActor
- `moonpool/src/simulations/spacesim/model.rs` — Reference model
- `moonpool/src/simulations/spacesim/invariants.rs` — Conservation, non-negative, directory integrity
- `moonpool/src/simulations/spacesim/operations.rs` — Operation alphabet
- `moonpool/src/simulations/spacesim/workloads.rs` — SpaceProcess + SpaceWorkload
- `moonpool/src/bin/sim/spacesim.rs` — Binary entry point

**Modify**:
- `moonpool/src/simulations/mod.rs` — Replace `banking` with `spacesim`
- `xtask/src/main.rs` — Replace `sim-banking-chaos` with `sim-spacesim`, sancov_crates: `moonpool,moonpool_transport`

## Shared infrastructure pattern

All processes and workloads share `Rc`-based resources (safe because moonpool is single-threaded `!Send`):

```rust
let membership = Rc::new(SharedMembership::new());
let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
let state_store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());
let cluster = ClusterConfig::builder()
    .name("spacesim")
    .membership(membership.clone())
    .directory(directory.clone())
    .build()?;
```

Passed into `Process` factories and `Workload` constructors via closure capture / constructor args.

---

## [x] Commit 1: `feat(sim): scaffold spacesim with single StationActor on one process`

**Goal**: Prove actors work inside a `Process` in simulation. Delete banking sim.

**Config**: 1 process, 1 workload, `NetworkConfiguration::fast_local()`, no attrition, 100 iterations.

**StationActor** (`actors.rs`):
```rust
#[service(id = 0x5741_7100)]
trait Station {
    async fn deposit_credits(&mut self, req: DepositCreditsRequest) -> Result<StationResponse, RpcError>;
    async fn withdraw_credits(&mut self, req: WithdrawCreditsRequest) -> Result<StationResponse, RpcError>;
    async fn query_state(&mut self, req: QueryStateRequest) -> Result<StationResponse, RpcError>;
}
```
- `PersistentState<StationData>` with `credits: i64`, `inventory: BTreeMap<String, i64>`
- `DeactivateOnIdle` for max lifecycle exercise
- `PlacementStrategy::Local` (single process for now)

**SpaceProcess** (`workloads.rs`): Creates `MoonpoolNode`, registers `StationActorImpl`, holds alive until shutdown.

**SpaceWorkload** (`workloads.rs`):
- `setup()`: Wait for process to register in membership
- `run()`: Create client-side `MoonpoolNode` (no actor registrations), seed 5 stations with random credits (1000..5000), run 200 random ops (deposit/withdraw/query), validate responses against reference model
- `check()`: Final conservation check

**Reference model** (`model.rs`): `SpaceModel { stations: BTreeMap<String, StationState>, total_credits: i64 }`. Published to `StateHandle` after each op.

**Invariants** (`invariants.rs`): `CreditConservation` + `NonNegativeBalances`.

**Operations** (`operations.rs`): Deposit (30%), Withdraw (25%), QueryState (25%), SmallDelay (20%). Model only updated on successful RPC response.

**Assertions**:
- `assert_always!(sum == expected, "credit conservation")` — in invariant
- `assert_always!(*balance >= 0, "non-negative credits")` — in invariant
- `assert_always!(resp.credits == model_credits, "response matches model")` — after each op
- `assert_sometimes!(true, "withdraw_rejected_insufficient")` — when station rejects
- `assert_sometimes!(true, "withdraw_succeeded")` — when withdrawal works

**xtask**: Replace `sim-banking-chaos` entry with `sim-spacesim`, sancov: `moonpool,moonpool_transport`.

---

## [ ] Commit 2: `feat(sim): add cargo tracking and VerifyAll operation to spacesim`

**Goal**: Extend StationActor with cargo operations. Add cross-verification that queries all stations and compares to model.

**StationActor gains**:
```rust
async fn add_cargo(&mut self, req: AddCargoRequest) -> Result<StationResponse, RpcError>;
async fn remove_cargo(&mut self, req: RemoveCargoRequest) -> Result<StationResponse, RpcError>;
```

**Model gains**: `total_cargo: BTreeMap<String, i64>` per commodity.

**New invariant**: `CargoConservation` — `sum(all station inventory[c]) == total_cargo[c]` for all commodities.

**New operations**: AddCargo (15%), RemoveCargo (15%), VerifyAll (5%). Rebalance weights.

**VerifyAll**: Queries every station and asserts response matches model exactly:
```rust
assert_always!(resp.credits == expected.credits, "verify: credit mismatch");
assert_always!(resp_cargo == expected_cargo, "verify: cargo mismatch");
```

**New assertions**:
- `assert_always!(cargo_sum == expected, "cargo conservation")` — in CargoConservation
- `assert_sometimes!(true, "remove_cargo_rejected")` — insufficient cargo
- `assert_sometimes!(true, "verify_all_passed")` — full verification succeeded

---

## [ ] Commit 3: `feat(sim): multi-process spacesim with 3 station nodes`

**Goal**: **First-ever multi-node actor simulation**. Highest-risk commit. 3 processes each hosting MoonpoolNode, actors placed via RoundRobin across all 3.

**Config**: 3 processes, 1 workload, `NetworkConfiguration::fast_local()`, no attrition. Reduce to 50 iterations initially.

**Changes**:
- `PlacementStrategy::RoundRobin` on StationActor
- Binary: `.processes(3, factory)`
- Workload `setup()`: Wait for all 3 processes to register in membership (poll loop with timeout)
- Workload creates its own client-side MoonpoolNode (also registers StationActorImpl so it can host actors too)

**New assertions**:
- `assert_reachable!("all_processes_registered")` — cluster formed successfully
- `assert_sometimes!(true, "cross_node_call")` — actor placed on different node from caller

**What might break**:
- Directory race conditions (two nodes activating same actor)
- Deadlock from workload waiting for processes
- Placement targeting nodes that haven't finished starting
- Deadlock detector false positives

**Debug approach**: If failing, use `IterationControl::FixedCount(1)` with specific seed and `RUST_LOG=error`.

---

## [ ] Commit 4: `feat(sim): add ShipActor with actor-to-actor trade calls`

**Goal**: Second actor type making cross-actor calls. Ship calls Station within its own dispatch handler.

**ShipActor** (`actors.rs`):
```rust
#[service(id = 0x5348_1900)]
trait Ship {
    async fn trade(&mut self, req: TradeRequest) -> Result<ShipResponse, RpcError>;
    async fn query_ship(&mut self, req: QueryShipRequest) -> Result<ShipResponse, RpcError>;
}
```
- `PersistentState<ShipData>` with `credits: i64`, `cargo: BTreeMap<String, i64>`, `docked_at: Option<String>`
- `DeactivateOnIdle`, `PlacementStrategy::RoundRobin`

**Trade operation**: Ship calls `station.remove_cargo()` then adds cargo locally (or vice versa for selling). Actor-to-actor call via `ctx.actor_ref::<StationRef<_>>(station_id)`.

**Process**: Registers both `StationActorImpl` and `ShipActorImpl`.

**Model gains**: `ships: BTreeMap<String, ShipState>`. Conservation now spans stations + ships.

**New operations**: Trade (20% of alphabet) — pick random ship, random station, random commodity, random buy/sell direction.

**Conservation invariants updated**: `sum(station_credits) + sum(ship_credits) == total_credits`, same for cargo.

**Partial failure handling**: If station call succeeds but ship state write fails, model tracks `lost_credits`/`lost_cargo`. Conservation: `sum + lost == total`.

**New assertions**:
- `assert_always!(total + lost == initial, "conservation with ships")`
- `assert_sometimes!(true, "trade_buy_succeeded")` — ship bought from station
- `assert_sometimes!(true, "trade_sell_succeeded")` — ship sold to station
- `assert_sometimes!(true, "trade_insufficient_cargo")` — station had no cargo

**What might break**: Deadlock in actor-to-actor calls (ship→station on same node), two-phase transfer atomicity.

---

## [ ] Commit 5: `feat(sim): enable network chaos for spacesim`

**Goal**: Turn on `random_network()`. RPC calls will fail with timeouts and connection errors.

**Config change**: `.random_network()` added to builder.

**Workload changes**: Every operation wrapped in error handling — on RPC failure, do NOT update model:
```rust
match actor_ref.deposit_credits(req).await {
    Ok(resp) => { model.deposit(...); assert_always!(...); }
    Err(_) => { assert_sometimes!(true, "deposit_rpc_failed"); }
}
```

**Transfer partial failure**: If withdraw succeeds but deposit fails (network error mid-transfer), credits are lost from station but never arrive at ship. Model records this in `lost_credits`. Conservation: `sum + lost == total`.

**New assertions**:
- `assert_sometimes!(true, "deposit_rpc_failed")` — network chaos causes failures
- `assert_sometimes!(true, "trade_partial_failure")` — half-completed trade
- `assert_sometimes!(true, "query_rpc_failed")` — queries fail too

**What might break**: Stale directory cache entries after connection failures, RPC timeouts too short for chaos latencies, forwarding bugs.

---

## [ ] Commit 6: `feat(sim): enable attrition with process reboots for spacesim`

**Goal**: Process crash/restart cycles. Actor state must survive via shared `InMemoryStateStore`. Actors re-register in directory after reboot.

**Config**:
```rust
.attrition(Attrition {
    max_dead: 1,
    prob_graceful: 0.4,
    prob_crash: 0.5,
    prob_wipe: 0.1,
    recovery_delay_ms: Some(500..3000),
    grace_period_ms: Some(1000..3000),
})
.phases(PhaseConfig {
    chaos_duration: Duration::from_secs(30),
    recovery_duration: Duration::from_secs(15),
})
```

**Process changes**: On shutdown signal, call `node.shutdown().await` for graceful deactivation.

**Workload changes**: Wrap operations in timeouts. Tolerate higher error rates during chaos phase.

**State recovery**: `InMemoryStateStore` is `Rc`-shared outside simulation — survives process reboots. On re-activation, `on_activate` loads last-persisted state.

**New assertions**:
- `assert_sometimes!(true, "rpc_failed_during_reboot")` — calls fail during process death
- `assert_sometimes!(true, "operation_timeout")` — timeouts during chaos
- `assert_always!(total + lost == initial, "conservation survives reboots")`

**What might break**: Directory retaining stale entries for dead nodes, membership not cleaning up crashed nodes, two activations of same actor during reboot window.

---

## [ ] Commit 7: `feat(sim): add DirectoryIntegrity invariant and buggify to spacesim`

**Goal**: Directory-level invariant checking + fault injection inside actor handlers. Polish for production.

**DirectoryIntegrity invariant**: Published from workload check phase — `directory.list_all()` snapshot. Invariant verifies:
- No duplicate `(actor_type, identity)` entries (single-activation guarantee)
- All entries point to active membership nodes (no stale references to dead nodes)

**Buggify points in actors**:
```rust
// In StationActor handlers:
if buggify!() { return Err(RpcError::Internal("buggified".into())); }
if buggify!() { ctx.time().sleep(Duration::from_millis(50)).await; } // slow write
```

**Production config**: 200 iterations, 200 ops per iteration.

**New assertions**:
- `assert_always!(!has_duplicates, "no duplicate activations")`
- `assert_always!(!has_stale_entries, "no stale directory entries")`
- `assert_sometimes!(true, "buggified_station_error")` — error path exercised
- `assert_sometimes!(true, "buggified_slow_write")` — slow path exercised

---

## Summary

| # | Commit | Procs | Network | Attrition | Actors | Key test |
|---|--------|-------|---------|-----------|--------|----------|
| 1 | Scaffold + single process | 1 | fast_local | No | Station | Actor lifecycle in Process |
| 2 | Cargo + VerifyAll | 1 | fast_local | No | Station | Model accuracy |
| 3 | Multi-process (3 nodes) | 3 | fast_local | No | Station | **First multi-node test** |
| 4 | ShipActor + actor-to-actor | 3 | fast_local | No | Station+Ship | Cross-actor RPC |
| 5 | Network chaos | 3 | random | No | Station+Ship | RPC failures |
| 6 | Attrition | 3 | random | max_dead=1 | Station+Ship | State recovery |
| 7 | Directory invariant + buggify | 3 | random | max_dead=1 | Station+Ship | Full coverage |

## Verification

After each commit:
```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run
cargo xtask sim run spacesim
```
