# Fork-Based Multiverse Exploration for moonpool-sim

## Context

moonpool-sim runs one seed per simulation. Finding bugs requiring N independent rare events costs P(e1) x P(e2) x ... x P(eN) seeds — exponential. Antithesis approach: checkpoint when interesting, fork(), branch with different seeds. Reduces cost from product to sum.

moonpool-sim is ideal for fork(): single-threaded, no real I/O, no file descriptors, no mutexes. After fork, child has complete COW copy of SimWorld. Reseeds RNG and continues — all future decisions diverge.

## Crate Structure

```
moonpool-explorer/  NEW (deps: libc only)  <- leaf, no moonpool knowledge
     ^
moonpool-sim/       (adds dep on moonpool-explorer)
     ^
moonpool/           (re-exports via moonpool-sim, no direct dep needed)
```

**No cycles.** moonpool-explorer never imports moonpool-sim/core. Talks to the RNG through 2 function pointers set at init:

```rust
// moonpool-explorer — the entire coupling surface
thread_local! {
    static RNG_GET_COUNT: Cell<fn() -> u64> = const { Cell::new(|| 0) };
    static RNG_RESEED: Cell<fn(u64)> = const { Cell::new(|_| {}) };
}
pub fn set_rng_hooks(get_count: fn() -> u64, reseed: fn(u64));
```

```rust
// moonpool-sim runner init — wires the hooks
moonpool_explorer::set_rng_hooks(
    get_rng_call_count,                                        // fn() -> u64
    |seed| { set_sim_seed(seed); reset_rng_call_count(); },   // fn(u64)
);
```

---

## PR 1: RNG Call Counting + Breakpoints ✅ DONE

**Goal**: Make the RNG replay-capable. Foundation for fork recording and deterministic replay.

**File**: `moonpool-sim/src/sim/rng.rs` (only file changed)

### Changes

Add two thread-locals:

```rust
thread_local! {
    static RNG_CALL_COUNT: Cell<u64> = const { Cell::new(0) };
    static RNG_BREAKPOINTS: RefCell<VecDeque<(u64, u64)>> = RefCell::new(VecDeque::new());
}
```

Add `check_rng_breakpoint()`:

```rust
fn check_rng_breakpoint() {
    RNG_BREAKPOINTS.with(|bp| {
        let mut breakpoints = bp.borrow_mut();
        while let Some(&(target_count, new_seed)) = breakpoints.front() {
            let count = RNG_CALL_COUNT.with(|c| c.get());
            if count > target_count {  // > not >= (fork fires BETWEEN calls)
                breakpoints.pop_front();
                SIM_RNG.with(|rng| *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(new_seed));
                CURRENT_SEED.with(|s| *s.borrow_mut() = new_seed);
                RNG_CALL_COUNT.with(|c| c.set(1)); // 1 not 0: current call IS first of new segment
            } else {
                break;
            }
        }
    });
}
```

Modify all 4 RNG functions (`sim_random`, `sim_random_range`, `sim_random_range_or_default`, `sim_random_f64`):
1. Increment `RNG_CALL_COUNT`
2. Call `check_rng_breakpoint()`
3. Sample (existing)

Add public API: `get_rng_call_count()`, `reset_rng_call_count()`, `set_rng_breakpoints()`, `clear_rng_breakpoints()`

Update `reset_sim_rng()` to also reset count and clear breakpoints.

### Tests

1. `test_call_counting` — N values -> count == N
2. `test_breakpoint_reseed` — breakpoint at count=5, verify calls 1-5 use old seed, 6+ use new
3. `test_chained_breakpoints` — multiple breakpoints in sequence
4. `test_replay_determinism` — record recipe, replay via breakpoints, verify identical output
5. `test_reset_clears_everything`

**~60 LOC. Zero breaking changes.**

---

## PR 2: moonpool-explorer Crate + Fork Loop

**Goal**: New `moonpool-explorer` crate with MAP_SHARED infrastructure, assertion-triggered forking, and the core fork loop. Wire into moonpool-sim.

### New crate: `moonpool-explorer/`

**`Cargo.toml`**: only dep is `libc = "0.2"`

#### `src/shared_mem.rs`
- `alloc_shared(size) -> Result<*mut u8, io::Error>` — `mmap(MAP_SHARED | MAP_ANONYMOUS)`
- `free_shared(ptr, size)`

#### `src/coverage.rs`
```rust
pub const COVERAGE_MAP_SIZE: usize = 1024;
pub struct CoverageBitmap { ptr: *mut u8 }  // per-child, cleared before fork
pub struct VirginMap { ptr: *mut u8 }        // cross-process, OR'd by all
```

#### `src/assertion_slots.rs`
```rust
pub const MAX_ASSERTION_SLOTS: usize = 128;
#[repr(C)]
pub struct AssertionSlot {
    pub name_hash: u64,
    pub fork_triggered: u8,    // CAS: 0->1
    pub pass_count: u64,
    pub fail_count: u64,
}
```
- `maybe_fork_on_assertion(name: &str)` — hash name, find/alloc slot, CAS fork_triggered, call `branch_on_discovery()`

#### `src/context.rs`
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
    pub recipe: Vec<(u64, u64)>,
    pub children_per_fork: u32,
}
```

#### `src/fork_loop.rs`
`branch_on_discovery(mark_name)`:
1. Check: active? depth < max_depth? energy > 0?
2. Record recipe segment via `RNG_GET_COUNT` hook
3. Save parent bitmap
4. For each child: decrement energy -> clear bitmap -> compute child_seed (FNV-1a) -> `fork()` -> CHILD: reseed via `RNG_RESEED` hook, set is_child, depth++, RETURN -> PARENT: `waitpid()`, merge coverage, check exit 42
5. Restore parent bitmap, pop recipe

#### `src/replay.rs`
- `format_timeline(&[(u64, u64)]) -> String`
- `parse_timeline(&str) -> Result<Vec<(u64, u64)>>`

#### `src/shared_stats.rs`
```rust
#[repr(C)]
pub struct SharedStats { global_energy: AtomicI64, total_timelines: AtomicU64, fork_points: AtomicU64, bug_found: AtomicU64 }
#[repr(C)]
pub struct SharedRecipe { claimed: AtomicU32, len: u32, entries: [(u64, u64); 128] }
```

#### `src/lib.rs`
- `ExplorationConfig { max_depth, children_per_fork, global_energy }`
- `set_rng_hooks(get_count, reseed)`
- `init(config)` / `cleanup()` / `explorer_is_child() -> bool`

### moonpool-sim changes

**`Cargo.toml`**: add `moonpool-explorer = { path = "../moonpool-explorer" }`

**`src/chaos/assertions.rs`** — add `on_sometimes_success()`:
```rust
pub fn on_sometimes_success(name: &str) {
    moonpool_explorer::maybe_fork_on_assertion(name);
}
```
Modify `sometimes_assert!` macro to call `$crate::chaos::assertions::on_sometimes_success(stringify!($name))` when result is true.

**`src/lib.rs`** — re-export explorer types

**`src/runner/builder.rs`** — add `enable_exploration(config)` builder method. In `run()`: call `set_rng_hooks()`, `moonpool_explorer::init()` before loop, `cleanup()` after.

**`src/runner/orchestrator.rs`** — at end of `orchestrate_workloads()`:
```rust
if moonpool_explorer::explorer_is_child() {
    let code = if results.iter().all(|r| r.is_ok()) { 0 } else { 42 };
    unsafe { libc::_exit(code); }
}
```

### Tests

1. `test_fork_basic` — workload with `sometimes_assert!`, verify SharedStats.total_timelines > 0
2. `test_child_exit_code` — child hits always_assert! failure, parent detects exit 42
3. `test_depth_limit` — max_depth=1, no grandchildren
4. `test_energy_limit` — global_energy=2, children_per_fork=8, only 2 forked
5. `test_replay_matches_fork` — capture recipe, replay with breakpoints, identical results
6. `test_exploration_disabled_default` — old behavior unchanged
7. `test_planted_bug` — nested rare conditions (mini-maze: 3 gates at P=0.1), exploration finds it, brute-force doesn't

**moonpool-explorer: ~600 LOC. moonpool-sim changes: ~40 LOC.**

---

## PR 3: Adaptive Forking + Energy Budgets + Reporting

**Goal**: Replace fixed-count with yield-driven adaptive forking. Add 3-level energy budgets. Produce exploration report.

### moonpool-explorer changes

**`src/fork_loop.rs`** — adaptive batch loop:
```rust
pub struct AdaptiveConfig { batch_size: u32, min_timelines: u32, max_timelines: u32 }
// Measure virgin edge yield per batch. Escalate if productive, collapse if barren.
```

**`src/energy.rs`** (new):
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

**`src/corpus.rs`** (new):
```rust
#[repr(C)]
pub struct Checkpoint { id: u32, mark_hash: u32, depth: u8, parent_seed: u64, ... }
pub const MAX_CHECKPOINTS: usize = 4096;
```

### moonpool-sim changes

**`src/runner/builder.rs`** — extend ExplorationConfig with adaptive fields
**`src/runner/report.rs`** — add `ExplorationReport { timelines, fork_points, bug_recipe, checkpoints }`

### Tests

1. `test_adaptive_collapses_barren`
2. `test_adaptive_escalates_productive`
3. `test_energy_realloc`
4. `test_exploration_report`
5. `test_checkpoint_tracking`

**~300 LOC.**

---

## Summary

```
PR1: RNG Breakpoints (~60 LOC)          moonpool-sim only
 |
PR2: Explorer crate + fork (~640 LOC)   NEW moonpool-explorer + moonpool-sim wiring
 |
PR3: Adaptive + report (~300 LOC)       moonpool-explorer + moonpool-sim report
```

Total: ~1000 lines across 3 PRs.

### Deferred
- `assert_sometimes_greater_than!` (watermark forking)
- `assert_sometimes_each!` (SOMETIMES_EACH bucketed)
- `assert_sometimes_all!` (frontier forking)
- Multi-core workers
- Coverage in `step()` (hash prev_event x curr_event)
- Buggify as branch points

### Verification
After each PR: `nix develop --command cargo fmt && nix develop --command cargo clippy && nix develop --command cargo nextest run`
