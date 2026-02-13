# Fork-Based Multiverse Exploration for moonpool-sim

## Context

moonpool-sim runs one seed per simulation. Finding bugs that require N independent rare events costs P(e1) x P(e2) x ... x P(eN) seeds — exponential in N. The Antithesis/maze-explorer approach: checkpoint when something interesting happens, fork(), branch children with different seeds. This reduces cost from product to sum of probabilities.

moonpool-sim is ideal for fork(): single-threaded, no real I/O, no file descriptors, no mutexes. After fork, the child has a complete COW copy of SimWorld (event queue, network, storage), the tokio runtime, and all thread-local state. It reseeds the RNG and continues stepping — all future decisions diverge.

The implementation ports the core mechanisms from the maze-explorer PoC into moonpool-sim's architecture, split into 3 incremental PRs.

---

## PR 1: RNG Call Counting + Breakpoints

**Goal**: Make the RNG replay-capable. This is the foundation for both fork recording and deterministic replay of discovered timelines.

**File**: `moonpool-sim/src/sim/rng.rs` (only file changed)

### Changes

Add two thread-locals alongside existing `SIM_RNG` and `CURRENT_SEED`:

```rust
thread_local! {
    static RNG_CALL_COUNT: Cell<u64> = const { Cell::new(0) };
    static RNG_BREAKPOINTS: RefCell<VecDeque<(u64, u64)>> = RefCell::new(VecDeque::new());
}
```

Add `check_rng_breakpoint()` (called after count increment, before sample):

```rust
fn check_rng_breakpoint() {
    RNG_BREAKPOINTS.with(|bp| {
        let mut breakpoints = bp.borrow_mut();
        while let Some(&(target_count, new_seed)) = breakpoints.front() {
            let count = RNG_CALL_COUNT.with(|c| c.get());
            if count > target_count {  // > not >= (PoC lesson: fork fires BETWEEN calls)
                breakpoints.pop_front();
                SIM_RNG.with(|rng| {
                    *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(new_seed);
                });
                CURRENT_SEED.with(|s| *s.borrow_mut() = new_seed);
                RNG_CALL_COUNT.with(|c| c.set(1)); // 1 not 0: current call IS first of new segment
            } else {
                break;
            }
        }
    });
}
```

Modify all 4 RNG consumption functions (`sim_random`, `sim_random_range`, `sim_random_range_or_default`, `sim_random_f64`) to:
1. Increment `RNG_CALL_COUNT`
2. Call `check_rng_breakpoint()`
3. Sample (existing code)

Add public API:
- `get_rng_call_count() -> u64`
- `reset_rng_call_count()`
- `set_rng_breakpoints(breakpoints: &[(u64, u64)])` — each entry is (target_count, new_seed)
- `clear_rng_breakpoints()`

Update `reset_sim_rng()` to also reset call count and clear breakpoints.

### Tests

1. `test_call_counting` — generate N values, verify count == N
2. `test_breakpoint_reseed` — breakpoint at count=5 with new seed. Verify calls 1-5 use old seed, calls 6+ use new seed
3. `test_chained_breakpoints` — multiple breakpoints, verify correct reseed chain
4. `test_replay_determinism` — record a sequence of (seed, count) pairs, then replay via breakpoints, verify identical output
5. `test_reset_clears_everything` — set breakpoints and advance count, reset, verify clean state

**~60 new lines of code. Zero breaking changes.**

---

## PR 2: Explorer Module + Fork Loop

**Goal**: Add MAP_SHARED infrastructure, assertion-triggered forking, and the core fork loop. After this PR, moonpool-sim can explore alternate timelines.

### New dependency

`moonpool-sim/Cargo.toml`: add `libc = "0.2"`

### New files

#### `moonpool-sim/src/explorer/mod.rs`
Module root. Re-exports key types.

#### `moonpool-sim/src/explorer/shared_mem.rs`
- `alloc_shared(size: usize) -> Result<*mut u8, std::io::Error>` — `mmap(MAP_SHARED | MAP_ANONYMOUS)`
- `free_shared(ptr: *mut u8, size: usize)` — `munmap`
- Atomic helper functions for casting raw pointers to `AtomicU8`/`AtomicI64`/`AtomicU64`

#### `moonpool-sim/src/explorer/coverage.rs`
```rust
pub const COVERAGE_MAP_SIZE: usize = 1024;

pub struct CoverageBitmap { ptr: *mut u8 }  // per-child, cleared before each fork
pub struct VirginMap { ptr: *mut u8 }        // cross-process, OR'd by all

impl CoverageBitmap {
    pub fn clear(&self);
    pub fn merge_into_virgin(&self, virgin: &VirginMap) -> usize; // returns new edges found
}
impl VirginMap {
    pub fn count_edges(&self) -> usize;
}
```

#### `moonpool-sim/src/explorer/context.rs`
```rust
thread_local! {
    static EXPLORER_CTX: RefCell<ExplorerCtx> = RefCell::new(ExplorerCtx::inactive());
}

pub struct ExplorerCtx {
    pub active: bool,
    pub is_child: bool,
    pub depth: u32,
    pub max_depth: u32,
    pub current_seed: u64,
    pub recipe: Vec<(u64, u64)>,         // (seed, rng_count_at_fork) per segment
    pub children_per_fork: u32,          // fixed-count for now
    pub global_energy: *mut AtomicI64,   // pointer into MAP_SHARED
}

pub fn explorer_init(seed: u64, config: &ExplorationConfig, energy_ptr: *mut AtomicI64);
pub fn explorer_is_active() -> bool;
pub fn explorer_is_child() -> bool;
pub fn explorer_get_recipe() -> Vec<(u64, u64)>;
```

#### `moonpool-sim/src/explorer/fork_loop.rs`
The core `branch_on_discovery()`:

```rust
pub fn branch_on_discovery(mark_name: &str) {
    // 1. Check: active? depth < max_depth? global_energy > 0?
    // 2. Record recipe segment: (current_seed, rng_call_count)
    // 3. Save parent bitmap
    // 4. For each child 0..children_per_fork:
    //    a. Atomic decrement global_energy, break if exhausted
    //    b. Clear coverage bitmap
    //    c. Compute child_seed = hash(parent_seed, depth, mark_hash, child_idx)
    //    d. fork()
    //    e. CHILD: set is_child, depth++, push recipe, reseed RNG, RETURN
    //    f. PARENT: waitpid(), merge coverage into virgin, check exit code 42
    // 5. Restore parent bitmap, pop recipe segment
}
```

`child_seed_for_mark()` — FNV-1a hash of (parent_seed, depth, mark_hash, child_index).

#### `moonpool-sim/src/explorer/replay.rs`
- `format_timeline(recipe: &[(u64, u64)]) -> String` — "seed0:count0,seed1:count1,..."
- `parse_timeline(s: &str) -> Result<Vec<(u64, u64)>, String>`

#### `moonpool-sim/src/explorer/shared_stats.rs`
```rust
#[repr(C)]
pub struct SharedStats {
    pub global_energy: AtomicI64,
    pub total_timelines: AtomicU64,
    pub fork_points: AtomicU64,
    pub bug_found: AtomicU64,        // 0 or 1
}

#[repr(C)]
pub struct SharedRecipe {
    pub claimed: AtomicU32,           // CAS guard: 0->1
    pub len: u32,
    pub entries: [(u64, u64); 128],   // (seed, rng_count) segments
}
```

### Modified files

#### `moonpool-sim/src/chaos/assertions.rs`
Add dual-path to `sometimes_assert!`:

```rust
#[macro_export]
macro_rules! sometimes_assert {
    ($name:ident, $condition:expr, $message:expr) => {
        let result = $condition;
        $crate::chaos::assertions::record_assertion(stringify!($name), result);
        if result {
            $crate::explorer::maybe_fork_on_assertion(stringify!($name));
        }
    };
}
```

New function `maybe_fork_on_assertion(name)`:
- Hash the name, look up/allocate a MAP_SHARED assertion slot
- CAS `fork_triggered` from false to true (fire-once)
- If CAS wins AND explorer active: call `branch_on_discovery(name)`

#### `moonpool-sim/src/chaos/mod.rs`
Add `pub mod explorer;` (or wire via lib.rs). Re-export key types.

#### `moonpool-sim/src/lib.rs`
Add `pub mod explorer;` module and re-exports.

#### `moonpool-sim/src/runner/builder.rs`
Add `ExplorationConfig` and builder method:

```rust
pub struct ExplorationConfig {
    pub max_depth: u32,              // default: 3
    pub children_per_fork: u32,      // default: 8
    pub global_energy: i64,          // default: 1024
}

impl SimulationBuilder {
    pub fn enable_exploration(mut self, config: ExplorationConfig) -> Self { ... }
}
```

In `run()`: if exploration enabled, allocate shared memory once before the loop, initialize explorer context per iteration, teardown after.

#### `moonpool-sim/src/runner/orchestrator.rs`
At the end of `orchestrate_workloads()`, before returning:

```rust
if crate::explorer::explorer_is_child() {
    let exit_code = if results.iter().all(|r| r.is_ok()) { 0 } else { 42 };
    unsafe { libc::_exit(exit_code); }
}
```

### Assertion Slots (MAP_SHARED)

```rust
#[repr(C)]
pub struct AssertionSlot {
    pub name_hash: u64,
    pub fork_triggered: u8,    // CAS: 0->1
    pub pass_count: u64,
    pub fail_count: u64,
}
pub const MAX_ASSERTION_SLOTS: usize = 128;
```

Located in `explorer/shared_stats.rs` or a dedicated `explorer/assertion_slots.rs`.

### Tests

1. `test_fork_basic` — workload with `sometimes_assert!(rare_event, cond)`. Enable exploration, verify SharedStats.total_timelines > 0
2. `test_child_exit_code` — workload that hits `always_assert!` failure in forked child. Verify parent detects exit code 42
3. `test_depth_limit` — set max_depth=1. Child also hits assertion. Verify no grandchildren (shared stats)
4. `test_energy_limit` — global_energy=2, children_per_fork=8. Verify only 2 children forked
5. `test_replay_matches_fork` — run exploration, capture recipe, replay with breakpoints, verify same assertion results
6. `test_exploration_disabled_default` — without enable_exploration(), old behavior unchanged
7. `test_planted_bug` — workload with nested rare conditions (mini-maze: 3 gates at P=0.1). Verify exploration finds all gates opened within reasonable energy budget, while brute-force (1000 seeds) does not

### Key safety notes
- Children call `libc::_exit()` (not `std::process::exit()`) to avoid running destructors
- Fork only happens between RNG sample boundaries (inside assertion macros, not mid-borrow)
- All MAP_SHARED access uses atomics with Relaxed ordering (sufficient: single parent waits on each child sequentially)

**~500 lines of new code. Breaking change: `sometimes_assert!` now forks when explorer active.**

---

## PR 3: Adaptive Forking + Energy Budgets + Reporting

**Goal**: Replace fixed-count forking with yield-driven adaptive decisions. Add per-fork-point energy budgets with reallocation. Produce an exploration report.

### Modified files

#### `moonpool-sim/src/explorer/fork_loop.rs`
Replace fixed-count loop with adaptive batching:

```rust
// Adaptive config
pub struct AdaptiveConfig {
    pub batch_size: u32,        // initial batch (default: 4)
    pub min_timelines: u32,     // minimum before early-stop (default: 8)
    pub max_timelines: u32,     // cap per fork point (default: 64)
}

// In branch_on_discovery():
let mut batch_remaining = config.batch_size;
let mut timelines_spawned = 0;
loop {
    let virgin_before = virgin.count_edges();
    // Fork batch_remaining children...
    timelines_spawned += batch_count;

    if timelines_spawned >= config.max_timelines { break; }

    let batch_yield = virgin.count_edges() - virgin_before;
    if batch_yield == 0 && timelines_spawned >= config.min_timelines {
        // Barren: collapse, donate savings to realloc pool
        break;
    }
    if batch_yield > 0 {
        batch_remaining = (batch_remaining * 2).min(config.max_timelines - timelines_spawned);
        // If mark budget depleted, try realloc from pool
    }
}
```

#### `moonpool-sim/src/explorer/energy.rs` (new file)
```rust
#[repr(C)]
pub struct EnergyBudget {
    pub global_remaining: AtomicI64,
    pub per_mark: [AtomicI64; MAX_ASSERTION_SLOTS],
    pub per_mark_initial: i64,
    pub realloc_pool: AtomicI64,
    pub realloc_fraction_pct: u8,
}
```

Three-level budgets (global, per-seed via reset, per-mark-slot). Reallocation pool receives savings from collapsed fork points.

#### `moonpool-sim/src/explorer/corpus.rs` (new file)
```rust
#[repr(C)]
pub struct Checkpoint {
    pub id: u32,
    pub mark_hash: u32,
    pub depth: u8,
    pub parent_seed: u64,
    pub rng_count_at_fork: u64,
    pub virgin_edges_before: u16,
    pub virgin_edges_after: u16,
    pub timelines_spawned: u16,
    pub collapsed: bool,
    pub depleted: bool,
}
pub const MAX_CHECKPOINTS: usize = 4096;
```

#### `moonpool-sim/src/runner/builder.rs`
Extend `ExplorationConfig` with adaptive fields:

```rust
pub struct ExplorationConfig {
    pub max_depth: u32,
    pub global_energy: i64,
    pub per_mark_energy: i64,
    pub adaptive: AdaptiveConfig,
    pub realloc_fraction_pct: u8,
}
```

#### `moonpool-sim/src/runner/report.rs`
Add `ExplorationReport`:

```rust
pub struct ExplorationReport {
    pub timelines_explored: u64,
    pub fork_points: u64,
    pub new_edges_discovered: u64,
    pub bug_found: bool,
    pub bug_recipe: Option<String>,
    pub checkpoints: Vec<Checkpoint>,
}
```

Wire into `SimulationReport` as `pub exploration: Option<ExplorationReport>`.

### Tests

1. `test_adaptive_collapses_barren` — fork point that produces no new edges. Verify timelines_spawned ~ min_timelines
2. `test_adaptive_escalates_productive` — fork point where each child discovers edges. Verify batch size doubles
3. `test_energy_realloc` — barren point donates savings, productive point draws from pool
4. `test_exploration_report` — full run, verify report fields populated
5. `test_checkpoint_tracking` — verify corpus records fork points with correct metadata

**~300 lines of new code.**

---

## Summary

```
PR1: RNG Breakpoints (~60 LOC)     -- foundation, no fork needed
 |
PR2: Explorer + Fork Loop (~500 LOC) -- the core capability
 |
PR3: Adaptive + Report (~300 LOC)   -- intelligence layer
```

Total: ~860 lines across 3 PRs. Each is independently testable.

### What's deferred (future PRs)
- `assert_sometimes_greater_than!` (watermark-based forking)
- `assert_sometimes_each!` (SOMETIMES_EACH bucketed exploration)
- `assert_sometimes_all!` (frontier-based forking)
- Multi-core workers (fork N worker processes, per-worker maps, merge)
- Coverage integration into `step()` (hash prev_event_type x curr_event_type)
- Buggify transformation (fault injection as branch points)

### Verification
After each PR, run: `nix develop --command cargo fmt && nix develop --command cargo clippy && nix develop --command cargo nextest run`
