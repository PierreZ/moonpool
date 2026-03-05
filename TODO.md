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

## [x] Commit 2: `feat(sim): add cargo tracking and VerifyAll operation to spacesim`

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

## [x] Commit 2.5: `feat(sim): add DirectoryConsistency invariant via StateHandle`

**Goal**: Real-time directory/membership/node consistency checking after every simulation event.

**Architecture**: `InMemoryDirectory`, `SharedMembership`, and `ActorHost` each hold `Option<StateHandle>`. When set, they publish state snapshots after every mutation. A reusable `DirectoryConsistency` invariant cross-checks all three.

**Published state**:
- `directory_entries` → `HashMap<ActorId, ActorAddress>` (after register/unregister)
- `membership_snapshot` → `MembershipSnapshot` (after register_node/update_status/add/remove)
- `node_actors:{addr}` → `HashSet<ActorId>` (after activation/deactivation)

**Invariant checks** (`simulations/invariants.rs`, reusable):
1. No directory entry points to a Dead node
2. Directory → Node: if directory says X is on N, node N has X active
3. Node → Directory: if node N has X active, directory points X to N
4. Single activation: no actor active on two nodes simultaneously

**Files changed**: `directory.rs`, `membership.rs`, `host.rs`, `lifecycle.rs`, `infrastructure/mod.rs`, `actors/mod.rs`, `simulations/mod.rs`, `simulations/invariants.rs` (new), `spacesim/invariants.rs`, `spacesim/workloads.rs`, `spacesim.rs`
---

## [x] Commit 2.6: various transport/actor fixes

- `fix(transport): self-notify connection task after write failure to enable reconnection`
- `feat(sim): add before_iteration hook and clear methods for multi-seed reset`
- `feat(actors): add MoonpoolClient for client-only actor runtime`
- `fix(actors): add RPC timeout to prevent deadlock on connection death`

---

## Current Situation (2026-03-05)

**Spacesim is blocked.** Two seeds fail (`15204012862878889900`, `3780034198488802454`) with model-vs-actual divergence. Root cause: the **maybe-delivered ambiguity** — transport chaos drops a response after the actor processed the request, workload sees `Err`, doesn't update model, but actor state is already persisted.

This is the fundamental at-most-once delivery problem (FDB error_code 1030: `request_maybe_delivered`). Before proceeding to Commit 3 (multi-process), moonpool's transport needs fdbrpc-level delivery semantics.

**New reference files added:**
- `docs/references/foundationdb/FailureMonitor.{h,actor.cpp}` — address/endpoint failure tracking
- `docs/references/foundationdb/HealthMonitor.{h,actor.cpp}` — connection closure tracking
- `docs/references/foundationdb/fdbrpc.h` — RequestStream with 3 delivery modes
- `docs/references/foundationdb/genericactors.actor.h` — waitValueOrSignal, sendCanceler, retryBrokenPromise
- `docs/analysis/foundationdb/fdbrpc-backport-guide.md` — Rust mapping guide

**What moonpool transport already has** (= FlowTransport basics):
- ✅ sendReliable (reliable queue, retransmit on reconnect)
- ✅ Peer connection management + reconnect with backoff
- ✅ Ping-based liveness detection
- ✅ ReplyPromise/ReplyFuture + endpoint routing
- ✅ BrokenPromise on Drop

**What's missing** (= fdbrpc layer):
- ❌ `peer.disconnect` signal — no exposed disconnect notification
- ❌ FailureMonitor (`notify_disconnect`, `on_disconnect_or_failure`)
- ❌ `request_maybe_delivered` error (MaybeDelivered vs NotDelivered)
- ❌ Reply queue closure on peer disconnect (ReplyFutures hang until 30s timeout)
- ❌ 3 delivery modes: `send()` / `try_get_reply()` / `get_reply()`
- ❌ Request IDs for dedup
- ❌ sendUnreliable exposed to RPC layer

**Next: Commit 2.7 series — implement fdbrpc backport**

---

## RPC Strategies (from FDB fdbrpc layer)

Reference for choosing delivery modes. See `docs/analysis/foundationdb/layer-3-fdbrpc.md` for full details.

### The 6 Strategies

1. **Idempotent-by-Design**: Formulate request as desired end state, not delta. Re-delivery harmless. Use `get_reply()`.
2. **Generation/Sequence Dedup**: Tag requests with monotonic counter. Server ignores old generations. Use `get_reply()`.
3. **Fire-and-Forget**: Use `send()` for messages where losing one is fine (heartbeats, notifications).
4. **Read-Before-Retry**: On `MaybeDelivered`, query server state to check if request succeeded before retrying. **This is the spacesim pattern.**
5. **Well-Known Endpoint + retryBrokenPromise**: For endpoints surviving restarts. Catch `BrokenPromise`, retry with jitter.
6. **Load-Balanced with AtMostOnce**: Multiple equivalent servers. `AtMostOnce=true` for commits (don't retry), `false` for reads.

### Decision Flowchart

```text
Is losing the message acceptable?
  YES → Strategy 3: send(), fire-and-forget
  NO ↓
Can you reformulate as "set state = X"?
  YES → Strategy 1: idempotent-by-design + get_reply()
  NO ↓
Can the server track per-client sequence numbers?
  YES → Strategy 2: generation dedup + get_reply()
  NO ↓
Can you read state after failure to check?
  YES → Strategy 4: try_get_reply() + read-before-retry
  NO ↓
Is the endpoint well-known (survives reboots)?
  YES → Strategy 5: retryBrokenPromise loop
  NO → Strategy 2 (add server state) or Strategy 6 (load balance)
```

### Delivery Mode → Strategy Mapping

| Mode | Guarantee | Transport | On Disconnect | Typical Strategy |
|------|-----------|-----------|---------------|-----------------|
| `send()` | Fire-and-forget | sendUnreliable | Silently lost | Strategy 3 |
| `try_get_reply()` | At-most-once | sendUnreliable | `MaybeDelivered` | Strategy 4, 6 |
| `get_reply()` | At-least-once | sendReliable | Retransmits | Strategy 1, 2, 5 |
| `get_reply_unless_failed_for(d)` | At-least-once + timeout | sendReliable | `MaybeDelivered` after d | Singleton RPCs |

---

## [ ] Commit 2.7a: `feat(transport): add peer disconnect signal`

**Goal**: Peer emits a signal when connection drops. Foundation for FailureMonitor and delivery modes.

**FDB ref**: `Peer::disconnect` (`FlowTransport.h:174`) — `Promise<Void>` fired by `connectionKeeper`.

**Design**: Add `disconnect_notify: Rc<Notify>` to `PeerSharedState`. Fire in `handle_connection_failure()` and `ConnectionLossBehavior::Exit` paths. Reset (new `Rc<Notify>`) on successful reconnection.

**Why `Rc<Notify>`**: Matches existing `data_to_send: Rc<Notify>` pattern. Works with `tokio::select!`. Multiple waiters supported.

**Files**:
- `moonpool-transport/src/peer/core.rs` — add `disconnect_notify` field, fire on failure, reset on reconnect

---

## [ ] Commit 2.7b: `feat(transport): add FailureMonitor for address/endpoint failure tracking`

**Goal**: Reactive failure tracking. Address-level (is machine reachable?) + endpoint-level (is endpoint permanently dead?).

**FDB ref**: `SimpleFailureMonitor` (`FailureMonitor.h:146`, `FailureMonitor.actor.cpp`)

**Design**: Concrete struct (not trait — KISS, one implementation). Owned by `NetTransport` in `TransportData`. Uses `Rc<RefCell<Vec<Waker>>>` pattern from `NetNotifiedQueue` for async watchers.

```rust
pub struct FailureMonitor {
    inner: RefCell<FailureMonitorInner>,
}
struct FailureMonitorInner {
    address_status: BTreeMap<String, FailureStatus>,          // Missing = Failed
    failed_endpoints: BTreeMap<(String, UID), FailedReason>,  // Permanent failures
    endpoint_watchers: BTreeMap<String, Vec<Waker>>,          // State change watchers
    disconnect_watchers: BTreeMap<String, Vec<Waker>>,        // Disconnect watchers
}
```

**Producer methods** (called by connection_task):
- `set_status(addr, Available|Failed)` — on connect/disconnect
- `notify_disconnect(addr)` — on connection drop, wakes all watchers for that address
- `endpoint_not_found(ep)` — on broken_promise, marks permanently failed

**Consumer methods** (called by delivery mode functions):
- `get_state(ep) -> FailureStatus` — failed if endpoint perm-failed OR address failed
- `permanently_failed(ep) -> bool`
- `on_disconnect_or_failure(ep) -> impl Future<Output=()>` — resolves on disconnect or permanent failure
- `on_state_changed(ep) -> impl Future<Output=()>` — resolves on any status change

**Watcher impl**: Custom `Future` that checks if already failed → `Ready`, else registers waker → `Pending`. Producer methods drain+wake all registered wakers.

**Integration**:
- Add `Rc<FailureMonitor>` to `TransportData`, create in builder, expose accessor
- Pass to `connection_task`: call `set_status(Available)` on connect, `notify_disconnect()` + `set_status(Failed)` on failure
- In `dispatch()`: call `endpoint_not_found()` when endpoint not found

**Files**:
- `moonpool-transport/src/rpc/failure_monitor.rs` — **CREATE** (~250 lines)
- `moonpool-transport/src/rpc/mod.rs` — add module + re-exports
- `moonpool-transport/src/rpc/net_transport.rs` — add FM to TransportData, wire to peers
- `moonpool-transport/src/peer/core.rs` — accept FM param, call set_status/notify_disconnect
- `moonpool-transport/src/lib.rs` — re-export FailureMonitor, FailureStatus

---

## [ ] Commit 2.7c: `feat(transport): add MaybeDelivered error and reply queue closure on disconnect`

**Goal**: Fast failure detection (~2s vs 30s) + explicit ambiguity error.

**FDB ref**: `request_maybe_delivered` (error 1030), `endStreamOnDisconnect` (`genericactors.actor.h:332`)

**New error variant**:
```rust
pub enum ReplyError {
    BrokenPromise,
    ConnectionFailed,
    Timeout,
    Serialization { message: String },
    EndpointNotFound,
    MaybeDelivered,  // NEW — FDB error 1030
}
```

**Reply queue closure on disconnect**: Track which reply queues are pending on which remote address. When `connection_reader` detects peer closure → close all reply queues for that address with `MaybeDelivered`.

Add to `TransportData`:
```rust
pending_replies: BTreeMap<String, Vec<Weak<dyn ReplyQueueCloser>>>,
```

Add `close_reason: Option<ReplyError>` to `NetNotifiedQueueInner` to distinguish "closed by drop" (ConnectionFailed) vs "closed by disconnect" (MaybeDelivered).

**Files**:
- `moonpool-transport/src/rpc/reply_error.rs` — add MaybeDelivered
- `moonpool-transport/src/rpc/net_transport.rs` — pending_replies tracking, close on disconnect
- `moonpool-transport/src/rpc/request.rs` — register pending reply on send
- `moonpool-transport/src/rpc/reply_future.rs` — return MaybeDelivered on external close
- `moonpool-transport/src/rpc/net_notified_queue.rs` — add close_reason

---

## [ ] Commit 2.7d: `feat(transport): add try_get_reply and send delivery modes`

**Goal**: The 4 FDB delivery modes as free functions.

**FDB ref**: `fdbrpc.h:727-895`, `genericactors.actor.h:362-431` (waitValueOrSignal, sendCanceler)

**4 functions** in new `delivery.rs`:

1. **`send()`** — fire-and-forget via sendUnreliable, no reply endpoint, no queue registration
2. **`try_get_reply()`** — at-most-once, sendUnreliable, race reply vs `fm.on_disconnect_or_failure()`
3. **`get_reply()`** — at-least-once via sendReliable (= current `send_request`, renamed for FDB alignment)
4. **`get_reply_unless_failed_for(timeout)`** — at-least-once + timeout, race `get_reply` vs `fm.on_failed_for()`

**`try_get_reply()` implementation** (the critical one):
```rust
let fm = transport.failure_monitor();
let disc = fm.on_disconnect_or_failure(destination);
if fm.get_state(destination) == FailureStatus::Failed {
    return Err(ReplyError::MaybeDelivered);
}
let reply_future = send_request_unreliable(transport, destination, request, codec)?;
tokio::select! {
    result = reply_future => match result {
        Ok(resp) => Ok(resp),
        Err(ReplyError::BrokenPromise) => { fm.endpoint_not_found(destination); Err(ReplyError::MaybeDelivered) }
        Err(e) => Err(e),
    },
    _ = disc => Err(ReplyError::MaybeDelivered),
}
```

**Files**:
- `moonpool-transport/src/rpc/delivery.rs` — **CREATE** (~200 lines)
- `moonpool-transport/src/rpc/request.rs` — add `send_request_unreliable` helper
- `moonpool-transport/src/rpc/mod.rs` — add module + re-exports
- `moonpool-transport/src/lib.rs` — re-export delivery functions

---

## [ ] Commit 2.8: `feat(sim): make spacesim RPC fault-aware with try_get_reply`

**Goal**: Update spacesim to use `try_get_reply()` + Strategy 4 (read-before-retry). Unblocks Commit 3.

**Pattern change** — 3-way error handling on all mutation RPCs:
```rust
match try_get_reply(transport, station_ep, deposit_req, codec).await {
    Ok(resp) => {
        model.deposit(station, amount);
        assert_always!(resp.credits == model.credits(station), "response matches model");
    }
    Err(ReplyError::MaybeDelivered) => {
        assert_sometimes!(true, "deposit_maybe_delivered");
        // Reconcile: query actual state
        match get_reply(transport, station_ep, query_req, codec)?.await {
            Ok(actual) => model.reconcile(station, actual),
            Err(_) => model.mark_uncertain(station),
        }
    }
    Err(_) => {
        assert_sometimes!(true, "deposit_not_delivered");
    }
}
```

**Model gains**: `reconcile(station_id, actual_state)` and `mark_uncertain(station_id)`.

**Proc macro**: Generate `try_*` client method variants that use `try_get_reply()` alongside existing methods that use `get_reply()`.

**Files**:
- `moonpool/src/simulations/spacesim/operations.rs` — 3-way error handling
- `moonpool/src/simulations/spacesim/model.rs` — add reconcile/mark_uncertain
- `moonpool/src/simulations/spacesim/workloads.rs` — use try_get_reply
- `moonpool-transport-derive/src/lib.rs` — generate try_* variants

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

## [ ] Commit 7: `feat(sim): add buggify to spacesim actor handlers`

**Goal**: Fault injection inside actor handlers. Polish for production.

**DirectoryIntegrity invariant**: ~~Done in Commit 2.5~~ — `DirectoryConsistency` invariant via `StateHandle` runs after every event (not just check phase).

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

| # | Commit | Scope | Key test |
|---|--------|-------|----------|
| 2.7a | Peer disconnect signal | moonpool-transport | Signal fires on connection loss |
| 2.7b | FailureMonitor | moonpool-transport | Watchers wake on disconnect |
| 2.7c | MaybeDelivered + reply queue closure | moonpool-transport | Fast failure (~2s vs 30s) |
| 2.7d | 4 delivery modes | moonpool-transport | try_get_reply returns MaybeDelivered |
| 2.8 | Spacesim fault-aware RPCs | moonpool (spacesim) | Model reconciliation after ambiguity |
| 3 | Multi-process (3 nodes) | moonpool (spacesim) | **First multi-node test** |
| 4 | ShipActor + actor-to-actor | moonpool (spacesim) | Cross-actor RPC |
| 5 | Network chaos | moonpool (spacesim) | RPC failures |
| 6 | Attrition | moonpool (spacesim) | State recovery |
| 7 | Directory invariant + buggify | moonpool (spacesim) | Full coverage |

## Verification

After each commit:
```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run
cargo xtask sim run spacesim
```
