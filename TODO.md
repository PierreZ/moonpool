# Moonpool-Sim DX Redesign

Single PR, three phases, each committed independently. Designed for autonomous Claude Code web execution.

## Golden Rule

**When stuck, resolve to the most simple, incremental solution.** Do not overthink or over-engineer. If something doesn't compile, find the smallest change that fixes it. If a design is unclear, pick the simplest option that works. Iterate.

## Prerequisites

Before ANY work:
1. Read `CLAUDE.md` for project constraints (no unwrap, no tokio direct calls, async_trait(?Send), etc.)
2. Read this `TODO.md` for full context
3. **After any context compaction**, re-read both `CLAUDE.md` and `TODO.md` to restore context

## Rules

- **Each phase ends with a commit** that passes `nix develop --command cargo fmt && nix develop --command cargo clippy && nix develop --command cargo nextest run`
- **Never break the build**. If you need to remove code that other files depend on, update all dependents in the same phase. Only delete once dependents are updated.
- **Each phase updates this `TODO.md`** with a status line marking the phase complete.

---

## Phase 1: Remove old tests, keep the build green

**Status**: NOT STARTED

**Goal**: Delete all simulation/chaos test files. Keep all library code compiling. The old macros, builder API, StateRegistry, and InvariantCheck remain for now (transport src depends on them).

### 1.1 Delete test files

These are standalone test targets — removing them does not break library compilation:

```
DELETE moonpool-sim/tests/chaos/assertions.rs
DELETE moonpool-sim/tests/exploration/tests.rs
DELETE moonpool-transport/tests/simulation/workloads.rs
DELETE moonpool-transport/tests/simulation/invariants.rs
DELETE moonpool-transport/tests/simulation/test_scenarios.rs
DELETE moonpool-transport/tests/e2e/workloads.rs
DELETE moonpool-transport/tests/e2e/invariants.rs
```

Keep mod.rs stubs / directory structure if needed for remaining test files to compile.

### 1.2 Remove builder inline tests

In `moonpool-sim/src/runner/builder.rs`: delete the `#[cfg(test)] mod tests { ... }` block at the bottom. The builder itself stays intact.

### 1.3 Verify + commit

```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run
```

Update this `TODO.md`: mark Phase 1 complete.

Commit: `refactor(moonpool): remove old simulation tests for DX redesign`

---

## Phase 2: Build new infrastructure, swap old APIs

**Status**: NOT STARTED

**Goal**: Add all new types, new macros, new builder/orchestrator. Port transport src macro calls. Delete old types. Everything compiles, no tests yet.

### 2.1 moonpool-explorer: EachBucket infrastructure

**Create `moonpool-explorer/src/each_buckets.rs`** (~200 LOC)

Adapt the source code below to moonpool-explorer's thread-local `Cell<*mut>` pattern (NOT global AtomicPtr like the original). Replace `assertion_branch(MAX_ASSERTIONS + bucket_index)` with `crate::fork_loop::dispatch_branch(name, bucket_idx % crate::assertion_slots::MAX_ASSERTION_SLOTS)`.

#### Source to port: Constants + EachBucket struct

```rust
pub const MAX_EACH_BUCKETS: usize = 256;
pub const MAX_EACH_KEYS: usize = 6;
const EACH_MSG_LEN: usize = 32;

/// One bucket's state in MAP_SHARED memory for SOMETIMES_EACH assertions.
/// Each unique combination of identity key values creates one bucket.
/// Optional quality watermark (`has_quality != 0`): re-forks when `best_score` improves.
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
        let len = self
            .msg
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(EACH_MSG_LEN);  // ADAPT: use .unwrap_or() not unwrap()
        std::str::from_utf8(&self.msg[..len]).unwrap_or("???")  // ADAPT: acceptable for display
    }
}
```

#### Source to port: msg_hash (FNV-1a u32)

```rust
/// FNV-1a hash of a message string -> stable u32 mark ID.
fn msg_hash(msg: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in msg.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}
```

#### Source to port: find_or_alloc_each_bucket

**ADAPTATION**: The original uses `EACH_BUCKET_PTR: AtomicPtr<u8>` (global static). In moonpool-explorer, use the thread-local `Cell<*mut u8>` pattern. The pointer is passed as an argument (read from thread-local at the call site).

```rust
/// Layout: [next_bucket: u32, _pad: u32, buckets: [EachBucket; MAX_EACH_BUCKETS]]

/// Find or allocate an EachBucket by (site_hash, bucket_hash).
fn find_or_alloc_each_bucket(
    ptr: *mut u8,
    site_hash: u32,
    bucket_hash: u32,
    keys: &[(&str, i64)],
    msg: &str,
    has_quality: u8,
) -> *mut EachBucket {
    unsafe {
        let next_atomic = &*(ptr as *const AtomicU32);
        let count = next_atomic.load(Ordering::Relaxed) as usize;
        let base = ptr.add(8) as *mut EachBucket;

        // Search existing buckets.
        for i in 0..count.min(MAX_EACH_BUCKETS) {
            let bucket = base.add(i);
            if (*bucket).site_hash == site_hash && (*bucket).bucket_hash == bucket_hash {
                return bucket;
            }
        }

        // Allocate new bucket atomically.
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

        std::ptr::write(
            bucket,
            EachBucket {
                site_hash,
                bucket_hash,
                fork_triggered: 0,
                num_keys: num_keys as u8,
                has_quality,
                _pad: 0,
                pass_count: 0,
                best_score: i64::MIN,
                key_values,
                msg: msg_buf,
            },
        );
        bucket
    }
}
```

#### Source to port: compute_each_bucket_index

**ADAPTATION**: Read base pointer from thread-local `EACH_BUCKET_PTR` instead of global AtomicPtr.

```rust
/// Compute 0-based index of an EachBucket from its pointer.
fn compute_each_bucket_index(base_ptr: *mut u8, bucket: *const EachBucket) -> usize {
    if base_ptr.is_null() {
        return 0;
    }
    let buckets_base = unsafe { base_ptr.add(8) } as usize;
    let offset = (bucket as usize).saturating_sub(buckets_base);
    offset / std::mem::size_of::<EachBucket>()
}
```

#### Source to port: pack_quality / unpack_quality

```rust
/// Pack quality key values into a single i64 for lexicographic comparison.
/// First key gets highest 16 bits (highest priority). Up to 4 quality keys.
fn pack_quality(quality: &[(&str, i64)]) -> i64 {
    let mut packed: i64 = 0;
    let n = quality.len().min(4);
    for (i, &(_, v)) in quality.iter().take(n).enumerate() {
        let shift = (3 - i) * 16;
        packed |= ((v as u16) as i64) << shift;
    }
    packed
}

/// Unpack quality i64 back into individual values for display.
pub fn unpack_quality(packed: i64, n: u8) -> Vec<i64> {
    (0..n as usize)
        .map(|i| {
            let shift = (3 - i) * 16;
            ((packed >> shift) as u16) as i64
        })
        .collect()
}
```

#### Source to port: assertion_sometimes_each (the main backing function)

**ADAPTATION**: Replace `EACH_BUCKET_PTR.load(Ordering::Relaxed)` with reading from the thread-local `EACH_BUCKET_PTR: Cell<*mut u8>`. Replace `assertion_branch(MAX_ASSERTIONS + bucket_index)` with `crate::fork_loop::dispatch_branch(name, bucket_idx % crate::assertion_slots::MAX_ASSERTION_SLOTS)` where `name` is the msg string.

```rust
/// Backing function for SOMETIMES_EACH assertions.
/// Each unique combination of identity key values creates one bucket; forks on first
/// discovery. Optional quality keys re-fork when the packed quality score improves.
pub fn assertion_sometimes_each(msg: &str, keys: &[(&str, i64)], quality: &[(&str, i64)]) {
    let ptr = EACH_BUCKET_PTR.load(Ordering::Relaxed);  // ADAPT: read from Cell thread-local
    if ptr.is_null() {
        return;
    }

    // Compute bucket hash: site_hash mixed with identity key values only via FNV-1a.
    // Quality values are NOT included in the hash — they're watermarks, not identity keys.
    let site_hash = msg_hash(msg);
    let mut bucket_hash = site_hash;
    for &(_, val) in keys {
        for b in val.to_le_bytes() {
            bucket_hash ^= b as u32;
            bucket_hash = bucket_hash.wrapping_mul(0x01000193);
        }
    }

    let has_quality = quality.len().min(4) as u8;
    let score = if has_quality > 0 {
        pack_quality(quality)
    } else {
        0
    };
    let bucket = find_or_alloc_each_bucket(ptr, site_hash, bucket_hash, keys, msg, has_quality);
    if bucket.is_null() {
        return;
    }

    unsafe {
        // Increment pass count.
        let count_atomic = &*((&(*bucket).pass_count) as *const u32 as *const AtomicU32);
        count_atomic.fetch_add(1, Ordering::Relaxed);

        // Fork on first discovery: CAS fork_triggered from 0 -> 1.
        let ft = &*((&(*bucket).fork_triggered) as *const u8 as *const AtomicU8);
        let first_discovery = ft
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();

        if first_discovery {
            // On first discovery, initialize best_score via atomic store if quality-tracked.
            if has_quality > 0 {
                let bs_atomic = &*((&(*bucket).best_score) as *const i64 as *const AtomicI64);
                bs_atomic.store(score, Ordering::Relaxed);
            }

            let bucket_index = compute_each_bucket_index(ptr, bucket);  // ADAPT: pass ptr
            // ADAPT: use dispatch_branch instead of assertion_branch
            crate::fork_loop::dispatch_branch(
                msg,
                bucket_index % crate::assertion_slots::MAX_ASSERTION_SLOTS,
            );
        } else if has_quality > 0 {
            // Not first discovery: check quality watermark improvement.
            // CAS loop on best_score.
            let bs_atomic = &*((&(*bucket).best_score) as *const i64 as *const AtomicI64);
            let mut current = bs_atomic.load(Ordering::Relaxed);
            loop {
                if score <= current {
                    break;
                }
                match bs_atomic.compare_exchange_weak(
                    current,
                    score,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let bucket_index = compute_each_bucket_index(ptr, bucket);
                        crate::fork_loop::dispatch_branch(
                            msg,
                            bucket_index % crate::assertion_slots::MAX_ASSERTION_SLOTS,
                        );
                        break;
                    }
                    Err(actual) => current = actual,
                }
            }
        }
    }
}
```

#### Source to port: each_bucket_read_all

```rust
/// Read all recorded SOMETIMES_EACH buckets from shared memory.
pub fn each_bucket_read_all() -> Vec<EachBucket> {
    let ptr = EACH_BUCKET_PTR.load(Ordering::Relaxed);  // ADAPT: read from Cell thread-local
    if ptr.is_null() {
        return Vec::new();
    }
    unsafe {
        let count = (*(ptr as *const u32)) as usize;
        let count = count.min(MAX_EACH_BUCKETS);
        let base = ptr.add(8) as *const EachBucket;
        (0..count).map(|i| std::ptr::read(base.add(i))).collect()
    }
}
```

#### Wiring changes

**Update `moonpool-explorer/src/context.rs`**: Add `EACH_BUCKET_PTR: Cell<*mut u8>` thread-local (same pattern as `COVERAGE_BITMAP_PTR`).

**Update `moonpool-explorer/src/lib.rs`**:
- Add `pub mod each_buckets`
- In `init()`: allocate shared memory for EachBuckets (size = `8 + MAX_EACH_BUCKETS * size_of::<EachBucket>()`), store pointer in `EACH_BUCKET_PTR` thread-local
- In `cleanup()`: free the EachBucket shared memory
- Re-export: `assertion_sometimes_each`, `each_bucket_read_all`, `EachBucket`

### 2.2 New assertion macros

**In `moonpool-sim/src/chaos/assertions.rs`**: Add new macros alongside old ones (old ones still needed by transport src until step 2.6):

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

/// Per-value bucketed sometimes assertion. Forks on first discovery of each unique
/// key combination. Optional quality watermarks re-fork on improvement.
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

Add `on_sometimes_each()` backing function in assertions.rs:
```rust
pub fn on_sometimes_each(msg: &str, keys: &[(&str, i64)], quality: &[(&str, i64)]) {
    moonpool_explorer::assertion_sometimes_each(msg, keys, quality);
}
```

### 2.3 New simulation types (all additive, nothing breaks)

**Create `moonpool-sim/src/chaos/state_handle.rs`**:

`StateHandle` — `Rc<RefCell<HashMap<String, Box<dyn Any>>>>` with:
- `publish<T: Any + 'static>(key, value)` — insert/replace
- `get<T: Any + Clone>(key) -> Option<T>` — downcast + clone (can't return `&T` through `RefCell`)
- `contains(key) -> bool`

**Create `moonpool-sim/src/chaos/invariant_trait.rs`**:

`Invariant` trait:
```rust
pub trait Invariant {
    fn name(&self) -> &str;
    fn check(&self, state: &StateHandle, sim_time: u64);
}
```
Plus `invariant_fn(name, closure)` adapter that returns `Box<dyn Invariant>`.

**Create `moonpool-sim/src/runner/context.rs`**:

`SimContext` wrapping `SimProviders` + `WorkloadTopology` + `StateHandle` + `CancellationToken`:
- `providers() -> &SimProviders`
- `network() -> &SimNetworkProvider` (via providers)
- `time() -> &SimTimeProvider` (via providers)
- `task() -> &SimTaskProvider` (via providers — note: check actual type name)
- `random() -> &SimRandomProvider` (via providers)
- `storage() -> &SimStorageProvider` (via providers — if available)
- `my_ip() -> NetworkAddress`
- `peer() -> NetworkAddress` (first peer, convenience)
- `peers() -> &[NetworkAddress]`
- `shutdown() -> CancellationToken`
- `state() -> &StateHandle`

`CancellationToken`: Simple `Rc<Cell<bool>>` with `cancel()` and `is_cancelled()`. Or use `tokio_util::sync::CancellationToken` if available. Simplest: `Rc<Cell<bool>>`.

**Create `moonpool-sim/src/runner/workload.rs`**:

```rust
#[async_trait(?Send)]
pub trait Workload {
    fn name(&self) -> &str;
    async fn setup(&self, ctx: &SimContext) -> Result<(), SimulationError> { Ok(()) }
    async fn run(&self, ctx: &SimContext) -> Result<(), SimulationError>;
    async fn check(&self, ctx: &SimContext) -> Result<(), SimulationError> { Ok(()) }
}
```
Plus `FnWorkload` closure adapter and `workload_fn(name, closure)` helper.

**Create `moonpool-sim/src/runner/fault_injector.rs`**:

```rust
#[async_trait(?Send)]
pub trait FaultInjector {
    fn name(&self) -> &str;
    async fn inject(&self, ctx: &SimContext) -> Result<(), SimulationError>;
}
```

Add `PhaseConfig { chaos_duration_ms: u64, liveness_duration_ms: u64 }` (in builder.rs or separate file).

### 2.4 Rewrite builder

Replace `SimulationBuilder` internals:
- Remove `WorkloadFn` type alias
- Remove `register_workload()`, `with_invariants()`
- Fields become: `workloads: Vec<Box<dyn Workload>>`, `invariants: Vec<Box<dyn Invariant>>`, `fault_injectors: Vec<Box<dyn FaultInjector>>`, `phase_config: Option<PhaseConfig>`
- New methods: `workload(impl Workload)`, `workload_fn(name, closure)`, `invariant(impl Invariant)`, `invariant_fn(name, closure)`, `fault(impl FaultInjector)`, `phases(PhaseConfig)`, `random_network()` (rename of `use_random_config`), `until_all_sometimes_reached(n)`
- Keep: `set_iterations`, `set_debug_seeds`, `set_time_limit`, `enable_exploration`, `run()`

### 2.5 Rewrite orchestrator

New lifecycle in `orchestrate_workloads()`:
1. Create shared `StateHandle`
2. Create `SimContext` per workload (unique IP, shared StateHandle + shutdown)
3. **Setup phase**: `workload.setup(ctx)` sequentially for all workloads
4. **Run phase**: `spawn_local` all `workload.run(ctx)` + fault injectors concurrently. Event loop: `sim.step()` -> `Invariant::check(state, time)` -> yield. If `PhaseConfig`: cancel faults after chaos_duration, continue for liveness_duration. Without: first completion triggers shutdown.
5. **Check phase**: after all runs + `sim.run_until_empty()`, call `workload.check(ctx)` sequentially
6. Explorer child exit (same as current)

Keep `IterationManager`, `MetricsCollector`, `DeadlockDetector` — they handle iteration logic.

### 2.6 Port moonpool-transport/src macro calls

Mechanical replacement in ~6 files (~33 calls total):
- `always_assert!(name, cond, msg)` -> `assert_always!(cond, msg)`
- `sometimes_assert!(name, cond, msg)` -> `assert_sometimes!(cond, msg)`

Files: `rpc/net_transport.rs`, `peer/core.rs`, `rpc/net_notified_queue.rs`, `rpc/endpoint_map.rs`, `rpc/reply_promise.rs`, `rpc/reply_future.rs`.

### 2.7 Delete old code (now safe — no dependents remain)

- Delete `always_assert!` and `sometimes_assert!` macro definitions from assertions.rs
- Delete `moonpool-sim/src/chaos/state_registry.rs`
- Delete `moonpool-sim/src/chaos/invariants.rs`
- Remove `state_registry` field from `WorkloadTopology`
- Update `moonpool-sim/src/chaos/mod.rs`: remove old modules/re-exports, add new ones
- Update `moonpool-sim/src/runner/mod.rs`: add new modules/re-exports
- Update `moonpool-sim/src/lib.rs`: swap re-exports
- Update `moonpool/src/lib.rs`: update facade re-exports

### 2.8 Verify + commit

```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run
```

Update this `TODO.md`: mark Phase 2 complete.

Commit: `feat(moonpool-sim): new DX — SimContext, Workload trait, assert_sometimes_each!, new builder`

---

## Phase 3: New FDB-style workloads

**Status**: NOT STARTED

**Goal**: Replace deleted tests with FDB-style alphabet workloads using the new API. Add exploration fork points throughout the transport source code. Patterns from the blog post: operation alphabets (Normal/Adversarial/Nemesis), reference model invariants (BTreeMap), conservation laws.

### 3.1 Add exploration fork points in transport source code

Place `assert_sometimes!` and `assert_sometimes_each!` throughout `moonpool-transport/src/` to create checkpoints that trigger forking into different timelines during exploration. These go in the **library source code** (not tests) so every simulation run hits them.

#### `moonpool-transport/src/peer/core.rs` — Connection & Reliability

| Location | Type | What to assert |
|----------|------|----------------|
| Connection attempt fails (timeout/error) | `assert_sometimes_each!` | `"connection_failure", [("failure_count", state.reconnect_state.failure_count as i64)]` — fork per retry depth |
| Backoff applied before reconnect | `assert_sometimes!` | `backoff_delay > Duration::ZERO, "backoff_applied"` — explore backoff paths |
| Checksum mismatch detected | `assert_sometimes!` | `true, "checksum_corruption_detected"` — already on error path, just mark it |
| Reliable message requeued on write failure | `assert_sometimes!` | `true, "reliable_requeue_on_failure"` |
| Unreliable packets discarded (queue full) | `assert_sometimes!` | `true, "unreliable_discarded"` |
| Connection recovered after failures | `assert_sometimes!` | `failure_count > 0, "connection_recovered_after_failure"` |
| Buggified write failure triggered | `assert_sometimes!` | `true, "buggified_write_failure"` |
| Queue drain pattern | `assert_sometimes_each!` | `"queue_drain", [("reliable_len", reliable_q.len() as i64), ("unreliable_len", unreliable_q.len() as i64)]` — fork per queue state |
| Graceful connection close detected on read | `assert_sometimes!` | `true, "graceful_close_on_read"` |

#### `moonpool-transport/src/rpc/net_transport.rs` — Routing & Dispatch

| Location | Type | What to assert |
|----------|------|----------------|
| Send path selection | `assert_sometimes_each!` | `"send_path", [("path", path_id)]` where path_id: 0=local_reliable, 1=local_unreliable, 2=remote_reliable, 3=remote_unreliable |
| Peer reused (outgoing) | `assert_sometimes!` | `true, "peer_reused_outgoing"` |
| Peer reused (incoming for response) | `assert_sometimes!` | `true, "peer_reused_incoming"` |
| New peer created | `assert_sometimes!` | `true, "new_peer_created"` |
| Dispatch undelivered (endpoint missing) | `assert_sometimes!` | `true, "dispatch_undelivered"` |
| Incoming connection accepted | `assert_sometimes!` | `true, "incoming_connection_accepted"` |
| Stale incoming peer replaced | `assert_sometimes!` | `true, "incoming_peer_replaced"` |

#### `moonpool-transport/src/rpc/reply_future.rs` — Response Resolution

| Location | Type | What to assert |
|----------|------|----------------|
| Reply resolution type | `assert_sometimes_each!` | `"rpc_resolution", [("type", type_id)]` where type_id: 0=immediate_success, 1=immediate_closed, 2=polled_success, 3=polled_closed |

#### `moonpool-transport/src/rpc/reply_promise.rs` — Promise Fulfillment

| Location | Type | What to assert |
|----------|------|----------------|
| Broken promise detected (Drop without send) | `assert_sometimes!` | `true, "broken_promise"` |
| Error reply sent | `assert_sometimes!` | `true, "error_reply_sent"` |

#### Guidance

- Each `assert_sometimes!` creates one fork point (forks on first `true`)
- Each `assert_sometimes_each!` creates one fork point per unique key combination (forks on first discovery of each bucket)
- Place assertions **after** the condition is known, not speculatively
- Don't add assertions inside tight loops — one per event/decision is enough
- The existing `always_assert!`/`sometimes_assert!` calls were already ported in Phase 2.6; this step adds **new** fork points that didn't exist before

### 3.2 Transport operation alphabet (test-side)

**`moonpool-transport/tests/simulation/alphabet.rs`** (NEW):

```rust
enum TransportOp {
    // Normal (70%): SendReliable, SendUnreliable, SendRpc, SmallDelay
    // Adversarial (20%): SendEmptyPayload, SendMaxSizePayload, SendToUnknownEndpoint
    // Nemesis (10%): UnregisterEndpoint, ReregisterEndpoint, DropRpcPromise
}

struct AlphabetWeights { normal: u32, adversarial: u32, nemesis: u32 }
fn pick_operation(ctx: &SimContext, weights: &AlphabetWeights) -> TransportOp
```

### 3.3 Reference model

**`moonpool-transport/tests/simulation/reference_model.rs`** (NEW):

BTreeMap-based reference model (deterministic iteration per FDB rules):
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

### 3.4 Invariants (preserving essence of deleted tests)

| Invariant | Type | Rule |
|-----------|------|------|
| No phantom reliable | assert_always! | `received ⊆ sent` |
| No phantom unreliable | assert_always! | `received ⊆ sent` |
| Unreliable conservation | assert_always! | `\|received\| <= \|sent\|` |
| No duplicate reliable | assert_always! | dup_count == 0 |
| RPC single resolution | assert_always! | responses, broken, timeouts disjoint |
| RPC no phantoms | assert_always! | response_ids ⊆ request_ids |
| All reliable delivered | assert_sometimes! (check phase) | received == sent |
| Some unreliable dropped | assert_sometimes! | \|received\| < \|sent\| |
| All unreliable delivered | assert_sometimes! | received == sent |
| RPC success path | assert_sometimes! | \|responses\| > 0 |
| RPC broken promise path | assert_sometimes! | \|broken\| > 0 |
| RPC timeout path | assert_sometimes! | \|timeouts\| > 0 |

### 3.5 Workload implementations

**`moonpool-transport/tests/simulation/workloads.rs`** (NEW):

`LocalDeliveryWorkload` — single-node, alphabet-driven, replaces old local + endpoint + RPC local workloads. Uses `Workload` trait with setup/run/check lifecycle.

`ServerWorkload` + `ClientWorkload` — multi-node RPC, replaces old server + client pair. Server: setup binds listener, run accepts+processes. Client: setup connects, run sends alphabet ops.

All workloads publish `TransportRefModel` to `ctx.state()` after each operation.

### 3.6 Test scenarios

**`moonpool-transport/tests/simulation/test_scenarios.rs`** (NEW):

```rust
// Fast (fixed seeds)
test_local_delivery_happy_path
test_local_delivery_adversarial
test_multi_node_rpc_1x1
test_multi_node_rpc_2x1

// Slow chaos (UntilAllSometimesReached)
slow_simulation_local_delivery
slow_simulation_multi_node_rpc
```

### 3.7 E2E workloads (same pattern)

**`moonpool-transport/tests/e2e/workloads.rs`** (NEW): Peer-level, wire format, connection lifecycle.

**`moonpool-transport/tests/e2e/invariants.rs`** (NEW): Reference model + ordering invariants.

### 3.8 moonpool-sim unit tests

**`moonpool-sim/tests/chaos/assertions.rs`** (NEW):
- `test_assert_always_success/failure`
- `test_assert_sometimes_tracking`
- `test_assert_sometimes_each_basic`
- `test_assert_sometimes_each_quality_watermark`

**`moonpool-sim/tests/exploration/tests.rs`** (NEW): Port exploration tests to new API:
- `test_fork_basic` — `workload_fn` + `assert_sometimes!`
- `test_depth_limit`, `test_energy_limit`
- `test_sometimes_each_triggers_fork`

### 3.9 Verify + commit

```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run
```

All tests pass.

Update this `TODO.md`: mark Phase 3 complete.

Commit: `feat(moonpool): FDB-style alphabet workloads with reference model invariants`

---

## Phase 4: Virtual actor simulation workloads

**Status**: NOT STARTED

**Goal**: Build FDB-style alphabet workloads for the virtual actor system in `moonpool/`. The actor system has rich state (Orleans-style lifecycle, persistent state with ETag concurrency, directory + placement, turn-based per-identity concurrency) — ideal for finding bugs with chaos + exploration.

### Actor system overview (for context)

Key types in `moonpool/src/actors/`:
- **`ActorHandler`** trait: `on_activate()`, `dispatch(method, body)`, `on_deactivate()`, `deactivation_hint()`
- **`ActorHost`**: Runtime that spawns routing loop per actor type + per-identity processing tasks
- **`ActorRouter`**: Caller-side — resolves actor location via directory, sends request, handles forwarding
- **`ActorContext`**: Passed to handlers — contains `ActorId`, `ActorRouter`, optional `ActorStateStore`
- **`PersistentState<T>`**: Typed ETag-guarded state wrapper — `load()`, `write_state()`, `clear_state()`
- **`ActorDirectory`** / **`InMemoryDirectory`**: Maps `ActorId -> Endpoint`
- **`PlacementStrategy`**: `LocalPlacement`, `RoundRobinPlacement`
- **`DeactivationHint`**: `KeepAlive`, `DeactivateOnIdle`, `DeactivateAfterIdle(Duration)`
- **`#[virtual_actor]`** proc macro: Generates typed refs + dispatch routing
- **IdentityMailbox**: `Rc<RefCell<VecDeque>>` + Waker, per-identity `!Send` channel
- **Don't call `stop().await` in sim workloads** — use `drop(host)` instead

Existing example: `moonpool/examples/banking.rs` (BankAccount with deposit/withdraw + PersistentState)

### 4.1 Actor operation alphabet

**`moonpool/tests/simulation/alphabet.rs`** (NEW):

```rust
enum ActorOp {
    // Normal (60%):
    Deposit { actor_id: String, amount: u64 },
    Withdraw { actor_id: String, amount: u64 },
    GetBalance { actor_id: String },
    Transfer { from: String, to: String, amount: u64 },

    // Adversarial (25%):
    SendToNonExistent { actor_id: String },
    InvalidMethod { actor_id: String, method: u32 },
    ConcurrentCallsSameActor { actor_id: String, count: u32 },
    ZeroAmountDeposit { actor_id: String },
    MaxAmountWithdraw { actor_id: String },

    // Nemesis (15%):
    DeactivateActor { actor_id: String },
    CorruptStateStore { actor_id: String },
    FloodSingleActor { actor_id: String, count: u32 },
}
```

Identity pool: N actors (e.g., 5-10 named "actor-0" through "actor-N"), picked randomly per operation. This creates natural contention + cross-actor interactions.

### 4.2 Actor reference model

**`moonpool/tests/simulation/reference_model.rs`** (NEW):

```rust
struct ActorRefModel {
    /// Expected balance per actor (ground truth)
    balances: BTreeMap<String, i64>,
    /// Total deposited across all actors
    total_deposited: u64,
    /// Total withdrawn across all actors
    total_withdrawn: u64,
    /// Successful operations per actor
    ops_per_actor: BTreeMap<String, u64>,
    /// Failed operations (insufficient funds, not found, etc.)
    failed_ops: BTreeMap<String, Vec<FailedOp>>,
    /// Activation count per actor
    activations: BTreeMap<String, u64>,
    /// Deactivation count per actor
    deactivations: BTreeMap<String, u64>,
}
```

### 4.3 Actor invariants

| Invariant | Type | Rule |
|-----------|------|------|
| Balance never negative | assert_always! | `balance >= 0` for all actors |
| Conservation law | assert_always! | `sum(balances) == total_deposited - total_withdrawn` |
| Activate before dispatch | assert_always! | activation_count >= 1 when ops > 0 |
| ETag prevents lost updates | assert_always! | no concurrent write succeeds without ETag match |
| Unknown method returns error | assert_always! | invalid method -> ActorError, not panic |
| Turn-based: no concurrent dispatch | assert_always! | only 1 dispatch active per identity at a time |
| Actor eventually activated | assert_sometimes! | activation_count > 0 for each actor |
| Actor deactivated on shutdown | assert_sometimes! | deactivation fires for active actors |
| DeactivateOnIdle exercised | assert_sometimes! | deactivate->reactivate cycle observed |
| Insufficient funds rejected | assert_sometimes! | withdraw > balance fails gracefully |
| Transfer completes | assert_sometimes! | cross-actor transfer succeeds |
| Concurrent calls serialized | assert_sometimes! | multiple concurrent calls all complete |

### 4.4 Actor fork points in source code

Place `assert_sometimes!` and `assert_sometimes_each!` in `moonpool/src/actors/`:

| File | Location | Type | What to assert |
|------|----------|------|----------------|
| `host.rs` | Actor activated (identity_processing_loop) | `assert_sometimes_each!` | `"actor_activated", [("type", actor_type.0 as i64)]` |
| `host.rs` | Actor deactivated | `assert_sometimes!` | `"actor_deactivated"` |
| `host.rs` | DeactivateOnIdle triggered | `assert_sometimes!` | `"deactivate_on_idle"` |
| `host.rs` | DeactivateAfterIdle timeout | `assert_sometimes!` | `"deactivate_after_idle_timeout"` |
| `host.rs` | Message forwarded (not local) | `assert_sometimes!` | `"actor_message_forwarded"` |
| `host.rs` | Mailbox closed (shutdown path) | `assert_sometimes!` | `"identity_mailbox_closed"` |
| `router.rs` | Directory cache hit | `assert_sometimes!` | `"directory_cache_hit"` |
| `router.rs` | Placement strategy invoked | `assert_sometimes!` | `"placement_invoked"` |
| `router.rs` | Cache invalidation received | `assert_sometimes!` | `"cache_invalidation"` |
| `state.rs` | ETag mismatch on write | `assert_sometimes!` | `"etag_conflict"` |
| `persistent_state.rs` | State loaded from store | `assert_sometimes!` | `"state_loaded"` |
| `persistent_state.rs` | State written to store | `assert_sometimes!` | `"state_persisted"` |

### 4.5 Actor workload implementations

**`moonpool/tests/simulation/workloads.rs`** (NEW):

`BankingWorkload` — single-node, alphabet-driven:
- **setup**: Create `ActorHost` with `InMemoryStateStore`, register `BankAccountImpl`, create router
- **run**: Loop picking random `ActorOp`, execute via `BankAccountRef`, update `ActorRefModel` in `ctx.state()`, check invariants inline
- **check**: Verify conservation law, verify all actors can respond to `GetBalance`, verify final balances match reference model

`MultiNodeBankingWorkload` — multi-node with `RoundRobinPlacement`:
- **setup**: Each node creates `ActorHost` + router with shared `InMemoryDirectory`
- **run**: Same alphabet but actors may be on different nodes (forwarding exercised)
- **check**: Same invariants, plus verify directory consistency

### 4.6 Actor test scenarios

**`moonpool/tests/simulation/test_scenarios.rs`** (NEW):

```rust
// Fast (fixed seeds)
test_banking_happy_path           // Normal ops only
test_banking_adversarial          // Include adversarial ops
test_banking_nemesis              // Full alphabet
test_multi_node_banking_1x2       // 1 client, 2 actor nodes

// Slow chaos (UntilAllSometimesReached)
slow_simulation_banking           // All sometimes assertions covered
slow_simulation_multi_node_banking
```

### 4.7 Verify + commit

```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run
```

Update this `TODO.md`: mark Phase 4 complete.

Commit: `feat(moonpool): virtual actor simulation workloads with conservation law invariants`

---

## Existing code to reuse

- `SimProviders` at `moonpool-sim/src/providers/sim_providers.rs` — wraps all providers, use inside SimContext
- `WorkloadTopology` at `moonpool-sim/src/runner/topology.rs` — keep, remove `state_registry` field
- `TopologyFactory::create_topology()` — adapt (no StateRegistry param)
- `IterationManager`, `MetricsCollector`, `DeadlockDetector` in orchestrator — keep for iteration logic
- `SimulationReport`, `ExplorationReport` in `runner/report.rs` — keep unchanged
- `AssertionStats`, `record_assertion`, `get_assertion_results` — keep for stats tracking
- `buggify!`, `buggify_with_prob!` — keep unchanged

## Deferred

- `assert_sometimes_greater_than!` (watermark forking) — see Appendix B
- `assert_sometimes_all!` (frontier forking) — see Appendix B
- Multi-core workers
- Coverage in `step()` (hash prev_event x curr_event)
- Buggify as branch points

## Verification

After each phase: `nix develop --command cargo fmt && nix develop --command cargo clippy && nix develop --command cargo nextest run`

---

## Appendix A: Assertion usage patterns from dungeon.rs

Reference patterns showing how `assert_sometimes!` and `assert_sometimes_each!` are used in a real workload. Adapt these patterns for transport and actor workloads.

### Pattern 1: Per-value bucketed fork points with quality watermarks

Fork once per floor, re-fork if health improves (higher health = better starting position for exploration):

```rust
// When player is on the key tile, fork to explore different seeds at this point
if game.on_key_tile() && !game.has_key() {
    assert_sometimes_each!(
        "on key tile",
        [("floor", level as i64)],           // identity keys: one bucket per floor
        [("health", hp_bucket as i64)]        // quality: re-fork if health improves
    );
    if random_bool(KEY_FIND_P) {
        game.grant_key();
        assert_sometimes!(true, "key found");
    }
}
```

### Pattern 2: Outcome-driven fork points

Fork on each distinct outcome type (first discovery per type triggers exploration):

```rust
match outcome {
    StepOutcome::Descended => {
        assert_sometimes_each!(
            "descended",
            [("to_floor", new_level as i64)],
            [("health", hp_bucket as i64)]
        );
    }
    StepOutcome::Won => {
        assert_sometimes!(true, "treasure found");
    }
    StepOutcome::MissingKey => {
        assert_sometimes!(true, "stairs without key");
    }
    StepOutcome::Damaged => {
        assert_sometimes!(true, "survived monster hit");
    }
    StepOutcome::Healed => {
        assert_sometimes!(true, "picked up potion");
    }
    StepOutcome::EnteredRoom => {
        assert_sometimes_each!(
            "room explored",
            [("floor", level as i64), ("room", room_idx as i64)],
            [("has_key", has_key as i64), ("health", hp_bucket as i64)]
        );
    }
    _ => {}
}
```

### Pattern 3: Gate amplification

Fork at the gate (probability check), so children start right before the rare event with different seeds:

```rust
// Fork point BEFORE the probability gate — children resume here with fresh seeds
assert_sometimes_each!(
    "stairs with key",
    [("floor", level as i64)],
    [("health", hp_bucket as i64)]
);
// The probability gate that children will hit with different RNG outcomes:
if random_bool(RARE_EVENT_P) {
    // rare path now much more likely to be explored
}
```

### Key takeaway for moonpool workloads

- Place `assert_sometimes_each!` **before** probability gates (not after) — the fork happens at the assertion, children diverge on subsequent RNG calls
- Use identity keys for bucketing: `[("actor_id", id), ("retry_count", n)]`
- Use quality keys for watermarks: `[("queue_depth", depth)]` — deeper queue = more interesting starting point
- `assert_sometimes!` for boolean events: "this happened at least once"
- Both types create fork points that the explorer uses to branch into alternate timelines

---

## Appendix B: Deferred assertion types (source from Claude-fork-testing)

These assertion types exist in the reference implementation and can be added to moonpool-explorer later. Source preserved here for future implementation.

### assert_sometimes_greater_than! (numeric watermark forking)

Forks when the observed value improves (higher = better). Uses CAS loop on watermark field.

```rust
/// Backing function for numeric assertions.
/// Tracks watermark (best observed value). For NumericSometimes, forks when
/// fork_watermark improves (CAS loop).
pub fn assertion_numeric(cmp: AssertCmp, left: i64, right: i64, msg: &str) {
    // ... slot lookup ...
    // Watermark tracking:
    let maximize = matches!(cmp, AssertCmp::Gt | AssertCmp::Ge);
    let wm_atomic = &*((&(*slot).watermark) as *const i64 as *const AtomicI64);
    let mut current = wm_atomic.load(Ordering::Relaxed);
    loop {
        let dominated = if maximize { left <= current } else { left >= current };
        if dominated { break; }
        match wm_atomic.compare_exchange_weak(current, left, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
    // For NumericSometimes: separate fork_watermark with same CAS pattern
    // Fork triggers on fork_watermark improvement
}

// Macro:
#[macro_export]
macro_rules! assert_sometimes_greater_than {
    ($left:expr, $right:expr, $msg:expr) => {
        $crate::assertion_numeric(AssertCmp::Gt, $left as i64, $right as i64, $msg)
    };
}
```

### assert_sometimes_all! (frontier forking)

Forks when the count of simultaneously-true booleans increases (frontier advances).

```rust
/// Frontier assertion: fork when more booleans are simultaneously true than ever before.
pub fn assertion_sometimes_all(msg: &str, conditions: &[(&str, bool)]) {
    // ... slot lookup ...
    let count = conditions.iter().filter(|(_, b)| *b).count() as u8;
    // CAS loop on frontier field:
    let frontier_atomic = &*((&(*slot).frontier) as *const u8 as *const AtomicU8);
    let mut current = frontier_atomic.load(Ordering::Relaxed);
    loop {
        if count <= current { break; }
        match frontier_atomic.compare_exchange_weak(current, count, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                // Frontier advanced! Fork to explore.
                assertion_branch(slot_index);
                break;
            }
            Err(actual) => current = actual,
        }
    }
}

// Macro:
#[macro_export]
macro_rules! assert_sometimes_all {
    ($msg:expr, [ $(($name:expr, $val:expr)),+ $(,)? ]) => {
        $crate::assertion_sometimes_all($msg, &[ $(($name, $val)),+ ])
    };
}
```

### AssertKind variants for these

```rust
#[repr(u8)]
pub enum AssertKind {
    Always = 0,
    AlwaysOrUnreachable = 1,
    Sometimes = 2,
    Reachable = 3,
    Unreachable = 4,
    NumericAlways = 5,
    NumericSometimes = 6,
    BooleanSometimesAll = 7,
}

pub enum AssertCmp { Gt, Ge, Lt, Le }
```

### AssertionSlot extended fields (for numeric + frontier)

```rust
#[repr(C)]
pub struct AssertionSlot {
    pub msg_hash: u32,
    pub kind: u8,
    pub must_hit: u8,
    pub maximize: u8,          // 1 = Gt/Ge, 0 = Lt/Le
    pub _pad: u8,
    pub pass_count: u64,
    pub fail_count: u64,
    pub watermark: i64,        // best observed value (numeric)
    pub fork_watermark: i64,   // best value that triggered fork (NumericSometimes)
    pub frontier: u8,          // max simultaneous true count (BooleanSometimesAll)
    pub fork_triggered: u8,    // CAS guard for Sometimes/Reachable first fire
    pub _pad2: [u8; 6],
    pub msg: [u8; 64],
}
```
