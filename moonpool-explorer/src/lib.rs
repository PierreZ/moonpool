//! Multiverse exploration for deterministic simulation testing.
//!
//! This crate discovers rare bugs by splitting simulation timelines at key
//! moments. When your simulation reaches an interesting state for the first
//! time (like a retry path firing, or a timeout being hit), the explorer
//! forks the process and re-runs from that point with different randomness,
//! creating a tree of alternate timelines.
//!
//! # Glossary
//!
//! ```text
//! Term              What it means
//! ────────────────  ──────────────────────────────────────────────────────
//! Seed              A u64 that completely determines a simulation's randomness.
//!                   Same seed = same coin flips = same execution every time.
//!
//! Timeline          One complete simulation run. A seed + a sequence of
//!                   splitpoints uniquely identifies a timeline.
//!
//! Splitpoint        A moment where the explorer decides to branch the
//!                   multiverse. Happens when a `sometimes` assertion
//!                   succeeds for the first time, a numeric watermark
//!                   improves, or a frontier advances.
//!
//! Multiverse        The tree of all timelines explored from one root seed.
//!                   Each splitpoint creates new children with different seeds.
//!
//! Coverage bitmap   A small bitfield (8192 bits) that records which assertion
//!                   paths a timeline touched. Used to detect whether a
//!                   new timeline discovered anything its siblings didn't.
//!
//! Explored map      The union of all coverage bitmaps across all timelines.
//!                   Lives in shared memory. "Has this ever been seen?"
//!
//! Energy budget     A finite pool that limits how many timelines the explorer
//!                   can spawn. Prevents runaway forking.
//!
//! Watermark         The best numeric value ever observed for a given assertion.
//!                   Improving the watermark triggers a new splitpoint.
//!
//! Frontier          For compound boolean assertions: the max number of
//!                   conditions simultaneously true. Advancing it triggers
//!                   a splitpoint.
//!
//! Recipe            The sequence of splitpoints that leads to a specific
//!                   timeline. If a bug is found, its recipe lets you
//!                   replay exactly how to reach it.
//!
//! Mark              An assertion site that can trigger splitpoints.
//!                   Each mark has a name, a shared-memory slot,
//!                   and (in adaptive mode) its own energy allowance.
//! ```
//!
//! # The big idea
//!
//! A normal simulation picks one seed and runs one timeline:
//!
//! ```text
//! Seed 42 ──────────────────────────────────────────────────▶ done
//!   RNG call #1    #2    #3    #4   ...   #500
//! ```
//!
//! But bugs hide in rare states. Maybe a retry only fires 5% of the time,
//! and the bug only appears when two retries happen in the same run. With
//! single-seed testing, you might run thousands of seeds and never hit it.
//!
//! Multiverse exploration changes the strategy. Instead of running many
//! independent seeds, it dives deeper into promising seeds:
//!
//! ```text
//! Seed 42 ─────────────┬── splitpoint! ─────────────────────▶ done
//!   RNG call #1 ... #200│
//!                       │
//!                       ├── Timeline A (new seed) ──────────▶ done
//!                       ├── Timeline B (new seed) ──┬───────▶ done
//!                       │                           │
//!                       │                    nested splitpoint!
//!                       │                           │
//!                       │                           ├── Timeline B1 ──▶ done
//!                       │                           └── Timeline B2 ──▶ BUG!
//!                       │
//!                       └── Timeline C (new seed) ──────────▶ done
//! ```
//!
//! The key insight: if a seed reached an interesting state (the retry fired),
//! running from that point with different randomness is more likely to find
//! a second interesting event than starting from scratch.
//!
//! # How the simulation seed works
//!
//! Every decision in the simulation (latency, failure, timeout) comes from
//! a deterministic random number generator (RNG) seeded with a single `u64`.
//! The RNG is a counter: every call to it increments a call count.
//!
//! ```text
//! seed = 42
//! RNG call #1 → 0.73  (used for: network latency)
//! RNG call #2 → 0.12  (used for: should this connection fail?)
//! RNG call #3 → 0.89  (used for: timer jitter)
//! ...
//! RNG call #N → 0.41  (used for: partition probability)
//! ```
//!
//! Same seed = same sequence = same execution. This is what makes bugs
//! reproducible.
//!
//! # What triggers a splitpoint?
//!
//! Not every assertion triggers exploration. Only *discovery* assertions do,
//! and only when they discover something new:
//!
//! ```text
//! Assertion kind          Triggers a splitpoint when...
//! ──────────────────────  ────────────────────────────────────────────
//! assert_sometimes!       The condition is true for the FIRST time
//! assert_reachable!       The code path is reached for the FIRST time
//! assert_sometimes_gt!    The observed value beats the previous watermark
//! assert_sometimes_all!   More conditions are true simultaneously than ever
//! assert_sometimes_each!  A new identity-key combination is seen, or
//!                         the quality score improves for a known one
//!
//! assert_always!          NEVER (invariant, not a discovery)
//! assert_unreachable!     NEVER (safety check, not a discovery)
//! ```
//!
//! The "first time" check uses a CAS (compare-and-swap) on a `split_triggered`
//! flag in shared memory. Once a mark has triggered, it won't trigger again
//! (except for numeric watermarks and frontiers, which can trigger multiple
//! times as they improve).
//!
//! # How a splitpoint works (step by step)
//!
//! When `assert_sometimes!(retry_fired, "server retried request")` is `true`
//! for the first time at RNG call #200 with seed 42:
//!
//! ```text
//!  Step 1: CAS split_triggered from 0 → 1        (first-time guard)
//!  Step 2: Record splitpoint position              (RNG call count = 200)
//!  Step 3: Check energy budget                     (can we afford to split?)
//!  Step 4: Save parent's coverage bitmap to stack  (we'll restore it later)
//!
//!  For each new timeline (e.g., timelines_per_split = 3):
//!  ┌──────────────────────────────────────────────────────────────────┐
//!  │ Step 5: Clear child coverage bitmap                             │
//!  │ Step 6: Compute child seed = FNV-1a(parent_seed, mark, index)   │
//!  │ Step 7: fork()  ←── OS-level process fork (copy-on-write)       │
//!  │                                                                  │
//!  │   CHILD (pid=0):                                                │
//!  │     - Reseed RNG with child_seed                                │
//!  │     - Set is_child = true, depth += 1                           │
//!  │     - Append (200, child_seed) to recipe                        │
//!  │     - Return from split function                                │
//!  │     - Continue simulation with NEW randomness ─────▶ eventually │
//!  │       calls exit_child(0) or exit_child(42) if bug              │
//!  │                                                                  │
//!  │   PARENT (pid>0):                                               │
//!  │     Sequential mode:                                            │
//!  │       - waitpid(pid) — blocks until THIS child finishes         │
//!  │       - Merge child's coverage bitmap into explored map         │
//!  │       - If child exited with code 42 → record bug recipe        │
//!  │       - Loop to next child                                      │
//!  │     Parallel mode:                                              │
//!  │       - Track child in active set                               │
//!  │       - When slots are full → reap_one() via waitpid(-1)        │
//!  │       - Continue forking until all children spawned             │
//!  │       - Drain remaining active children at the end              │
//!  └──────────────────────────────────────────────────────────────────┘
//!
//!  Step 8: Restore parent's coverage bitmap
//!  Step 9: Parent continues its own simulation normally
//! ```
//!
//! The child seed is deterministic: `FNV-1a(parent_seed + mark_name + child_index)`.
//! So the multiverse tree is fully reproducible.
//!
//! # The coverage bitmap: "did this timeline find anything new?"
//!
//! Each timeline gets a small bitmap (1024 bytes = 8192 bits). When an
//! assertion fires, it sets a bit at position `hash(assertion_name) % 8192`:
//!
//! ```text
//! Bitmap for Timeline A:
//! byte 0          byte 1          byte 2
//! ┌─┬─┬─┬─┬─┬─┬─┬─┐┌─┬─┬─┬─┬─┬─┬─┬─┐┌─┬─┬─┬─┬─┬─┬─┬─┐
//! │0│0│1│0│0│0│0│0││0│0│0│0│0│1│0│0││0│0│0│0│0│0│0│0│ ...
//! └─┴─┴─┴─┴─┴─┴─┴─┘└─┴─┴─┴─┴─┴─┴─┴─┘└─┴─┴─┴─┴─┴─┴─┴─┘
//!       ▲                       ▲
//!       bit 2                   bit 13
//!       (retry_fired)           (timeout_hit)
//! ```
//!
//! The **explored map** is the union (OR) of all bitmaps across all timelines.
//! It lives in `MAP_SHARED` memory so all forked processes can see it:
//!
//! ```text
//! After Timeline A:   explored = 00100000 00000100 00000000 ...
//! After Timeline B:   explored = 00100000 00000110 00000000 ...
//!                                                 ▲
//!                                      Timeline B found bit 14!
//!                                      (that's new coverage)
//! ```
//!
//! **How "has new bits" works:**
//!
//! For each byte: `(child_byte & !explored_byte) != 0`
//!
//! ```text
//! child    = 00000110     (bits 1, 2 set)
//! explored = 00000100     (bit 2 already known)
//! !explored = 11111011
//! child & !explored = 00000010  ← bit 1 is NEW!  → has_new_bits = true
//! ```
//!
//! This is used in **adaptive mode** to decide whether a mark is still
//! productive (finding new paths) or barren (just repeating known paths).
//!
//! # The energy budget: "how much exploring can we do?"
//!
//! Without a limit, the explorer could fork forever (exponential blowup).
//! The energy budget caps the total number of timelines spawned.
//!
//! ## Fixed-count mode (simple)
//!
//! One global counter. Each timeline costs 1 energy. When it reaches 0,
//! no more timelines are spawned:
//!
//! ```text
//! global_energy: 10
//!
//! Split at mark A: spawn 3 timelines → energy = 7
//! Split at mark B: spawn 3 timelines → energy = 4
//! Split at mark C: spawn 3 timelines → energy = 1
//! Split at mark D: spawn 1 timeline  → energy = 0
//! Split at mark E: no energy left, skip
//! ```
//!
//! Configure with:
//! ```ignore
//! ExplorationConfig {
//!     max_depth: 2,
//!     timelines_per_split: 3,
//!     global_energy: 50,
//!     adaptive: None,      // ← fixed-count mode
//!     parallelism: None,   // ← sequential (or Some(Parallelism::MaxCores))
//! }
//! ```
//!
//! ## Adaptive mode (smart)
//!
//! A 3-level energy system that automatically gives more budget to productive
//! marks and takes budget away from barren ones:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │                    GLOBAL ENERGY (100)                       │
//! │  Every timeline consumes 1 from here first.                 │
//! │  When this hits 0, ALL exploration stops.                   │
//! ├──────────────────────────────────────────────────────────────┤
//! │                                                              │
//! │  ┌─────────────────┐  ┌─────────────────┐  ┌────────────┐  │
//! │  │  Mark A: 15     │  │  Mark B: 15     │  │ Mark C: 15 │  │
//! │  │  (productive)   │  │  (barren)       │  │ (new)      │  │
//! │  │  Used: 15/15    │  │  Used: 3/15     │  │ Used: 0/15 │  │
//! │  │  Needs more! ───┼──┼─── Returns 12 ──┼──┤            │  │
//! │  └────────┬────────┘  └─────────────────┘  └────────────┘  │
//! │           │                     │                            │
//! │           ▼                     ▼                            │
//! │  ┌──────────────────────────────────────────────────────┐   │
//! │  │           REALLOCATION POOL: 12                      │   │
//! │  │  Energy returned by barren marks.                    │   │
//! │  │  Productive marks draw from here when their          │   │
//! │  │  per-mark budget runs out.                           │   │
//! │  └──────────────────────────────────────────────────────┘   │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! **How it decides if a mark is productive or barren:**
//!
//! Timelines are spawned in batches (e.g., 4 at a time). After each batch,
//! the explorer checks the coverage bitmap:
//!
//! ```text
//! Mark "retry_path":
//!   Batch 1: spawn 4 timelines → check bitmap → 2 found new bits ✓
//!   Batch 2: spawn 4 timelines → check bitmap → 1 found new bits ✓
//!   Batch 3: spawn 4 timelines → check bitmap → 0 found new bits ✗
//!     → Mark is BARREN. Return remaining per-mark energy to pool.
//!     → Stop splitting at this mark.
//!
//! Mark "partition_heal":
//!   Batch 1: spawn 4 timelines → check bitmap → 3 found new bits ✓
//!   Batch 2: spawn 4 timelines → check bitmap → 2 found new bits ✓
//!   ...continues until per-mark budget exhausted...
//!   ...draws from reallocation pool for more...
//!   Batch 6: spawn 4 timelines → hits max_timelines (20) → stop
//! ```
//!
//! **The 3-level energy decrement (for each timeline):**
//!
//! ```text
//! 1. Decrement global_remaining by 1
//!    └─ If global is at 0 → STOP (hard cap, game over)
//!
//! 2. Decrement per_mark[slot] by 1
//!    └─ If per_mark has budget → OK, continue
//!    └─ If per_mark is at 0 → try step 3
//!
//! 3. Decrement realloc_pool by 1
//!    └─ If pool has budget → OK, continue
//!    └─ If pool is at 0 → UNDO global decrement, STOP for this mark
//! ```
//!
//! Configure with:
//! ```ignore
//! ExplorationConfig {
//!     max_depth: 3,
//!     timelines_per_split: 4,  // ignored in adaptive mode
//!     global_energy: 200,
//!     adaptive: Some(AdaptiveConfig {
//!         batch_size: 4,        // timelines per batch before checking yield
//!         min_timelines: 4,     // minimum timelines even if barren
//!         max_timelines: 20,    // hard cap per mark
//!         per_mark_energy: 15,  // initial budget per mark
//!     }),
//!     parallelism: Some(Parallelism::MaxCores),  // use all CPU cores
//! }
//! ```
//!
//! # Complete walkthrough: from seed to bug
//!
//! Say you're testing a distributed lock service with 3 nodes. Your test has:
//!
//! ```ignore
//! assert_sometimes!(lock_acquired_after_retry, "lock retry succeeded");
//! assert_sometimes!(saw_split_brain, "split brain detected");
//! assert_always!(no_double_grant, "lock never granted to two nodes");
//! ```
//!
//! **Iteration 1: Seed 42, exploration enabled**
//!
//! ```text
//! Root timeline (seed=42, depth=0)
//! │
//! │  RNG#1..#150: normal execution, no interesting events
//! │
//! │  RNG#151: lock_acquired_after_retry = true  ← FIRST TIME!
//! │           CAS split_triggered 0→1 succeeds
//! │           Splitpoint triggered at RNG call #151
//! │
//! ├── Timeline T0 (seed=FNV(42,"lock retry",0), depth=1)
//! │   │  Reseed RNG, continue from same program state
//! │   │  Different randomness → different network delays
//! │   │  RNG#1..#80: saw_split_brain = true  ← FIRST TIME (for children)
//! │   │  CAS split_triggered 0→1 succeeds
//! │   │  Nested splitpoint at RNG call #80!
//! │   │
//! │   ├── Timeline T0-0 (depth=2)
//! │   │     no_double_grant = false  ← BUG!
//! │   │     exit_child(42)  ← special code for "bug found"
//! │   │
//! │   ├── Timeline T0-1 (depth=2)
//! │   │     runs to completion, no bug
//! │   │     exit_child(0)
//! │   │
//! │   └── Timeline T0-2 (depth=2)
//! │         runs to completion, no bug
//! │         exit_child(0)
//! │   │
//! │   │  T0 continues after children finish
//! │   │  exit_child(0)
//! │
//! ├── Timeline T1 (seed=FNV(42,"lock retry",1), depth=1)
//! │     runs to completion, nothing new
//! │     exit_child(0)
//! │
//! └── Timeline T2 (seed=FNV(42,"lock retry",2), depth=1)
//!       runs to completion, nothing new
//!       exit_child(0)
//!
//! Root continues after all children finish.
//! Bug found! Recipe saved.
//! ```
//!
//! **The bug recipe** is the path from root to the buggy timeline:
//!
//! ```text
//! Recipe: [(151, seed_T0), (80, seed_T0_0)]
//! Formatted: "151@<seed_T0> -> 80@<seed_T0_0>"
//! ```
//!
//! To replay: set the root seed to 42, install RNG breakpoints at
//! call count 151 (reseed to seed_T0) and then at call count 80
//! (reseed to seed_T0_0). The simulation will follow the exact same
//! path to the bug every time.
//!
//! # How fork() makes this work
//!
//! The explorer uses Unix `fork()` to create timeline branches. This is
//! cheap because of copy-on-write (COW): the child gets a copy of the
//! parent's entire memory without actually copying it. Pages are only
//! copied when one side writes to them.
//!
//! ```text
//! Parent process memory:
//! ┌──────────────────────────────────────────────────┐
//! │  Simulation state (actors, network, timers)      │  ← COW pages
//! │  RNG state (will be reseeded in child)           │  ← COW pages
//! ├──────────────────────────────────────────────────┤
//! │  MAP_SHARED memory:                              │  ← truly shared
//! │    - Assertion table (128 slots)                 │
//! │    - Coverage bitmap pool (1 slot per core)      │
//! │    - Explored map (union of all coverage)        │
//! │    - Energy budget                               │
//! │    - Fork stats + bug recipe                     │
//! └──────────────────────────────────────────────────┘
//!                      │
//!                    fork()
//!                      │
//!            ┌─────────┴─────────┐
//!            ▼                   ▼
//!     Parent (reaps)      Child (continues)
//!     - waitpid()         - rng_reseed(new_seed)
//!     - merge coverage    - run simulation
//!     - check exit code   - exit_child(0 or 42)
//! ```
//!
//! `MAP_SHARED` memory is the only communication channel between parent
//! and child. Assertion counters, coverage bits, energy budgets, and bug
//! recipes all live there. This is why the crate only depends on `libc`.
//!
//! # Parallel exploration (multi-core)
//!
//! By default, the fork loop is sequential: fork one child, wait for it,
//! merge coverage, repeat. This uses only one CPU core.
//!
//! With `parallelism: Some(Parallelism::MaxCores)`, the explorer runs
//! multiple children concurrently using a **sliding window** capped at
//! the number of available CPU cores:
//!
//! ```text
//! Sequential (parallelism: None):
//!
//!   fork child 0 --- waitpid --- fork child 1 --- waitpid --- ...
//!   core 0 busy     core 0 idle  core 0 busy     core 0 idle
//!
//! Parallel (parallelism: Some(Parallelism::MaxCores)):
//!
//!   fork child 0 ──────────────────────── reap
//!   fork child 1 ─────────────────── reap
//!   fork child 2 ────────────── reap
//!   fork child 3 ─────── reap
//!   all cores busy until drained
//! ```
//!
//! The parent uses `waitpid(-1)` to reap whichever child finishes first,
//! then recycles that slot for the next child.
//!
//! ## How the bitmap pool works
//!
//! In sequential mode, children share one coverage bitmap (cleared between
//! each child). In parallel mode, concurrent children would clobber each
//! other's bitmap, so each child gets its own **bitmap pool slot**:
//!
//! ```text
//! Bitmap pool (MAP_SHARED, allocated lazily on first parallel split):
//! ┌─────────────┬─────────────┬─────────────┬─────────────┐
//! │  Slot 0     │  Slot 1     │  Slot 2     │  Slot 3     │
//! │  1024 bytes │  1024 bytes │  1024 bytes │  1024 bytes │
//! │  child A    │  child B    │  child C    │  (free)     │
//! └─────────────┴─────────────┴─────────────┴─────────────┘
//! ```
//!
//! When a child finishes (`waitpid(-1)` returns its PID), the parent:
//! 1. Looks up which slot that child used
//! 2. Merges the slot's bitmap into the explored map
//! 3. Recycles the slot for the next child
//!
//! Children that themselves become parents (nested splits) allocate their
//! own fresh pool -- the pool pointer is reset to null in each child at
//! fork time.
//!
//! ## Parallelism variants
//!
//! ```text
//! Variant              Slot count
//! ───────────────────  ─────────────────────────────────────
//! MaxCores             All available CPU cores
//! HalfCores            Half of available cores (rounded up, min 1)
//! Cores(n)             Exactly n cores
//! MaxCoresMinus(n)     All cores minus n (min 1)
//! ```
//!
//! ## Concurrency safety
//!
//! All `MAP_SHARED` state uses atomic operations that are safe across
//! `fork()` boundaries:
//! - **Assertion slots**: CAS for first-time discovery, `fetch_add` for counters
//! - **Energy budget**: `fetch_sub`/`fetch_add` with rollback on failure
//! - **Bug recipe**: CAS first-bug-wins -- only the first bug is recorded
//! - **Stats counters**: `fetch_add` -- concurrent increments are safe
//! - **Explored map merge**: Done by the parent AFTER `waitpid`, never concurrent
//!
//! # Architecture
//!
//! ```text
//! moonpool-explorer (this crate)  ── leaf, only depends on libc
//!      ▲
//! moonpool-sim                    ── wires RNG hooks at init
//! ```
//!
//! Communication with the simulation's RNG uses two function pointers
//! set via [`set_rng_hooks`]:
//! - `get_count: fn() -> u64` — how many RNG calls have happened
//! - `reseed: fn(u64)` — replace the RNG seed and reset the counter
//!
//! This crate has zero knowledge of moonpool internals. It doesn't know
//! about actors, networks, or storage. It only knows about RNG call counts,
//! seeds, and assertion slot positions.
//!
//! # Module overview
//!
//! ```text
//! lib.rs             ── ExplorationConfig, init/cleanup, this documentation
//! split_loop.rs      ── The fork loop: sequential + parallel sliding window,
//!                       adaptive batching, Parallelism enum
//! assertion_slots.rs ── Shared-memory assertion table (128 slots)
//! each_buckets.rs    ── Per-value bucketed assertions (256 buckets)
//! coverage.rs        ── CoverageBitmap + ExploredMap (8192-bit bitmaps)
//! energy.rs          ── 3-level energy budget (global + per-mark + realloc pool)
//! context.rs         ── Thread-local state, RNG hooks, bitmap pool pointers
//! shared_stats.rs    ── Cross-process counters (timelines, fork_points, bugs)
//! shared_mem.rs      ── mmap(MAP_SHARED|MAP_ANONYMOUS) allocation
//! replay.rs          ── Recipe formatting and parsing ("151@seed -> 80@seed")
//! ```
//!
//! # Usage
//!
//! ```ignore
//! // In moonpool-sim's runner:
//! moonpool_explorer::set_rng_hooks(get_rng_call_count, |seed| {
//!     set_sim_seed(seed);
//!     reset_rng_call_count();
//! });
//!
//! moonpool_explorer::init(ExplorationConfig {
//!     max_depth: 2,
//!     timelines_per_split: 4,
//!     global_energy: 100,
//!     adaptive: None,
//!     parallelism: Some(Parallelism::MaxCores),
//! })?;
//!
//! // ... run simulation ...
//!
//! moonpool_explorer::cleanup();
//! ```

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod assertion_slots;
pub mod context;
pub mod coverage;
pub mod each_buckets;
pub mod energy;
pub mod replay;
pub mod shared_mem;
pub mod shared_stats;
pub mod split_loop;

// Re-exports for the public API
pub use assertion_slots::{
    ASSERTION_TABLE_MEM_SIZE, AssertCmp, AssertKind, AssertionSlot, AssertionSlotSnapshot,
    assertion_bool, assertion_numeric, assertion_read_all, assertion_sometimes_all, msg_hash,
};
pub use context::{explorer_is_child, get_assertion_table_ptr, set_rng_hooks};
pub use each_buckets::{EachBucket, assertion_sometimes_each, each_bucket_read_all};
pub use replay::{ParseTimelineError, format_timeline, parse_timeline};
pub use shared_stats::{ExplorationStats, get_bug_recipe, get_exploration_stats};
pub use split_loop::{AdaptiveConfig, Parallelism, exit_child};

use context::{
    ASSERTION_TABLE, COVERAGE_BITMAP_PTR, EACH_BUCKET_PTR, ENERGY_BUDGET_PTR, EXPLORED_MAP_PTR,
    SHARED_RECIPE, SHARED_STATS,
};

/// Configuration for exploration.
#[derive(Debug, Clone)]
pub struct ExplorationConfig {
    /// Maximum fork depth (0 = no forking).
    pub max_depth: u32,
    /// Number of children to fork at each discovery point (fixed-count mode).
    pub timelines_per_split: u32,
    /// Global energy budget (total number of fork operations allowed).
    pub global_energy: i64,
    /// Optional adaptive forking configuration.
    /// When `None`, uses fixed `timelines_per_split` (backward compatible).
    /// When `Some`, uses coverage-yield-driven batch forking with 3-level energy.
    pub adaptive: Option<split_loop::AdaptiveConfig>,
    /// Optional parallelism for multi-core exploration.
    /// When `None`, children are forked and reaped sequentially (one core).
    /// When `Some`, a sliding window of concurrent children uses multiple cores.
    pub parallelism: Option<split_loop::Parallelism>,
}

/// Initialize assertion table and each-bucket shared memory only.
///
/// This allocates the shared memory regions needed for assertion tracking
/// without requiring a full exploration context. Idempotent (no-op if
/// already initialized).
///
/// # Errors
///
/// Returns an error if shared memory allocation fails.
pub fn init_assertions() -> Result<(), std::io::Error> {
    let current = ASSERTION_TABLE.with(|c| c.get());
    if !current.is_null() {
        return Ok(()); // Already initialized
    }

    let table_ptr = shared_mem::alloc_shared(assertion_slots::ASSERTION_TABLE_MEM_SIZE)?;
    let each_bucket_ptr = shared_mem::alloc_shared(each_buckets::EACH_BUCKET_MEM_SIZE)?;

    ASSERTION_TABLE.with(|c| c.set(table_ptr));
    EACH_BUCKET_PTR.with(|c| c.set(each_bucket_ptr));

    Ok(())
}

/// Free assertion table and each-bucket shared memory.
///
/// Nulls the pointers after freeing. No-op if not initialized.
pub fn cleanup_assertions() {
    unsafe {
        let table_ptr = ASSERTION_TABLE.with(|c| c.get());
        if !table_ptr.is_null() {
            shared_mem::free_shared(table_ptr, assertion_slots::ASSERTION_TABLE_MEM_SIZE);
            ASSERTION_TABLE.with(|c| c.set(std::ptr::null_mut()));
        }

        let each_bucket_ptr = EACH_BUCKET_PTR.with(|c| c.get());
        if !each_bucket_ptr.is_null() {
            shared_mem::free_shared(each_bucket_ptr, each_buckets::EACH_BUCKET_MEM_SIZE);
            EACH_BUCKET_PTR.with(|c| c.set(std::ptr::null_mut()));
        }
    }
}

/// Zero assertion table memory for between-run resets.
///
/// No-op if not initialized.
pub fn reset_assertions() {
    let table_ptr = ASSERTION_TABLE.with(|c| c.get());
    if !table_ptr.is_null() {
        unsafe {
            std::ptr::write_bytes(table_ptr, 0, assertion_slots::ASSERTION_TABLE_MEM_SIZE);
        }
    }

    let each_bucket_ptr = EACH_BUCKET_PTR.with(|c| c.get());
    if !each_bucket_ptr.is_null() {
        unsafe {
            std::ptr::write_bytes(each_bucket_ptr, 0, each_buckets::EACH_BUCKET_MEM_SIZE);
        }
    }
}

/// Prepare the exploration framework for the next seed in multi-seed exploration.
///
/// Preserves the explored map (cumulative coverage) and assertion watermarks/frontiers
/// so subsequent seeds benefit from prior discovery context. Resets per-seed transient
/// state: energy budgets, statistics, split triggers, pass/fail counters, coverage
/// bitmap, and bug recipe.
///
/// Call this between seeds instead of [`reset_assertions`] when you want
/// coverage-preserving multi-seed exploration.
pub fn prepare_next_seed(per_seed_energy: i64) {
    // 1. PRESERVE explored map — do nothing

    // 2. Selectively reset assertion table slots:
    //    PRESERVE: watermark, split_watermark, frontier (quality context)
    //    RESET: split_triggered, pass_count, fail_count (per-seed transient)
    let table_ptr = ASSERTION_TABLE.with(|c| c.get());
    if !table_ptr.is_null() {
        unsafe {
            let count_ptr = table_ptr as *const std::sync::atomic::AtomicU32;
            let count = (*count_ptr)
                .load(std::sync::atomic::Ordering::Relaxed)
                .min(assertion_slots::MAX_ASSERTION_SLOTS as u32) as usize;
            let base = table_ptr.add(8) as *mut assertion_slots::AssertionSlot;
            for i in 0..count {
                let slot = &mut *base.add(i);
                slot.split_triggered = 0;
                slot.pass_count = 0;
                slot.fail_count = 0;
            }
        }
    }

    // 3. Selectively reset each-bucket slots:
    //    PRESERVE: best_score (quality watermark), key_values, msg
    //    RESET: split_triggered, pass_count (per-seed transient)
    let each_ptr = EACH_BUCKET_PTR.with(|c| c.get());
    if !each_ptr.is_null() {
        unsafe {
            let count_ptr = each_ptr as *const std::sync::atomic::AtomicU32;
            let count = (*count_ptr)
                .load(std::sync::atomic::Ordering::Relaxed)
                .min(each_buckets::MAX_EACH_BUCKETS as u32) as usize;
            let base = each_ptr.add(8) as *mut each_buckets::EachBucket;
            for i in 0..count {
                let bucket = &mut *base.add(i);
                bucket.split_triggered = 0;
                bucket.pass_count = 0;
            }
        }
    }

    // 4. Zero coverage bitmap (per-timeline, NOT the explored map)
    let bm_ptr = COVERAGE_BITMAP_PTR.with(|c| c.get());
    if !bm_ptr.is_null() {
        unsafe {
            std::ptr::write_bytes(bm_ptr, 0, coverage::COVERAGE_MAP_SIZE);
        }
    }

    // 5. Reset energy budget with fresh per-seed energy
    let energy_ptr = ENERGY_BUDGET_PTR.with(|c| c.get());
    if !energy_ptr.is_null() {
        unsafe {
            energy::reset_energy_budget(energy_ptr, per_seed_energy);
        }
    }

    // 6. Reset shared stats
    let stats_ptr = SHARED_STATS.with(|c| c.get());
    if !stats_ptr.is_null() {
        unsafe {
            shared_stats::reset_shared_stats(stats_ptr, per_seed_energy);
        }
    }

    // 7. Reset bug recipe
    let recipe_ptr = SHARED_RECIPE.with(|c| c.get());
    if !recipe_ptr.is_null() {
        unsafe {
            shared_stats::reset_shared_recipe(recipe_ptr);
        }
    }

    // 8. Reset context for new seed, mark as warm start
    context::with_ctx_mut(|ctx| {
        ctx.is_child = false;
        ctx.depth = 0;
        ctx.current_seed = 0;
        ctx.recipe.clear();
        ctx.warm_start = true;
    });
}

/// Initialize the exploration framework.
///
/// Allocates shared memory for cross-process state and activates exploration.
/// Must be called after [`set_rng_hooks`] and before the simulation loop.
///
/// # Errors
///
/// Returns an error if shared memory allocation fails.
pub fn init(config: ExplorationConfig) -> Result<(), std::io::Error> {
    // Initialize assertion table first (idempotent)
    init_assertions()?;

    // Allocate exploration-specific shared memory regions
    let stats_ptr = shared_stats::init_shared_stats(config.global_energy)?;
    let recipe_ptr = shared_stats::init_shared_recipe()?;
    let explored_ptr = shared_mem::alloc_shared(coverage::COVERAGE_MAP_SIZE)?;
    let bitmap_ptr = shared_mem::alloc_shared(coverage::COVERAGE_MAP_SIZE)?;

    // Allocate energy budget if adaptive mode is configured
    let energy_ptr = if let Some(ref adaptive) = config.adaptive {
        energy::init_energy_budget(config.global_energy, adaptive.per_mark_energy)?
    } else {
        std::ptr::null_mut()
    };

    // Store pointers in thread-local context
    SHARED_STATS.with(|c| c.set(stats_ptr));
    SHARED_RECIPE.with(|c| c.set(recipe_ptr));
    EXPLORED_MAP_PTR.with(|c| c.set(explored_ptr));
    COVERAGE_BITMAP_PTR.with(|c| c.set(bitmap_ptr));
    ENERGY_BUDGET_PTR.with(|c| c.set(energy_ptr));

    // Activate exploration context
    context::with_ctx_mut(|ctx| {
        ctx.active = true;
        ctx.is_child = false;
        ctx.depth = 0;
        ctx.max_depth = config.max_depth;
        ctx.current_seed = 0;
        ctx.recipe.clear();
        ctx.timelines_per_split = config.timelines_per_split;
        ctx.adaptive = config.adaptive.clone();
        ctx.parallelism = config.parallelism.clone();
        ctx.warm_start = false;
    });

    Ok(())
}

/// Clean up the exploration framework.
///
/// Frees all shared memory and deactivates exploration.
/// Call after the simulation loop completes.
pub fn cleanup() {
    // Deactivate
    context::with_ctx_mut(|ctx| {
        ctx.active = false;
    });

    // Free exploration-specific shared memory regions
    // Safety: these pointers were allocated by init() via alloc_shared()
    unsafe {
        let stats_ptr = SHARED_STATS.with(|c| c.get());
        if !stats_ptr.is_null() {
            shared_mem::free_shared(
                stats_ptr as *mut u8,
                std::mem::size_of::<shared_stats::SharedStats>(),
            );
            SHARED_STATS.with(|c| c.set(std::ptr::null_mut()));
        }

        let recipe_ptr = SHARED_RECIPE.with(|c| c.get());
        if !recipe_ptr.is_null() {
            shared_mem::free_shared(
                recipe_ptr as *mut u8,
                std::mem::size_of::<shared_stats::SharedRecipe>(),
            );
            SHARED_RECIPE.with(|c| c.set(std::ptr::null_mut()));
        }

        let explored_ptr = EXPLORED_MAP_PTR.with(|c| c.get());
        if !explored_ptr.is_null() {
            shared_mem::free_shared(explored_ptr, coverage::COVERAGE_MAP_SIZE);
            EXPLORED_MAP_PTR.with(|c| c.set(std::ptr::null_mut()));
        }

        let bitmap_ptr = COVERAGE_BITMAP_PTR.with(|c| c.get());
        if !bitmap_ptr.is_null() {
            shared_mem::free_shared(bitmap_ptr, coverage::COVERAGE_MAP_SIZE);
            COVERAGE_BITMAP_PTR.with(|c| c.set(std::ptr::null_mut()));
        }

        let energy_ptr = ENERGY_BUDGET_PTR.with(|c| c.get());
        if !energy_ptr.is_null() {
            shared_mem::free_shared(
                energy_ptr as *mut u8,
                std::mem::size_of::<energy::EnergyBudget>(),
            );
            ENERGY_BUDGET_PTR.with(|c| c.set(std::ptr::null_mut()));
        }
    }

    // Clean up assertion table and each-buckets (shared with init_assertions)
    cleanup_assertions();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_cleanup_cycle() {
        let config = ExplorationConfig {
            max_depth: 2,
            timelines_per_split: 4,
            global_energy: 100,
            adaptive: None,
            parallelism: None,
        };

        init(config).expect("init failed");
        assert!(context::explorer_is_active());
        assert!(!context::explorer_is_child());

        // Stats should be available
        let stats = get_exploration_stats().expect("stats should be available");
        assert_eq!(stats.global_energy, 100);
        assert_eq!(stats.total_timelines, 0);
        assert_eq!(stats.fork_points, 0);
        assert_eq!(stats.bug_found, 0);

        cleanup();
        assert!(!context::explorer_is_active());
        assert!(get_exploration_stats().is_none());
    }

    #[test]
    fn test_inactive_by_default() {
        assert!(!context::explorer_is_active());
        assert!(!context::explorer_is_child());
    }

    #[test]
    fn test_assertion_bool_noop_when_inactive() {
        // Should not panic when assertion table is not initialized
        assertion_bool(AssertKind::Sometimes, true, true, "test_assertion");
    }

    #[test]
    fn test_init_assertions_standalone() {
        init_assertions().expect("init_assertions failed");

        // Table pointer should be set
        let ptr = context::get_assertion_table_ptr();
        assert!(!ptr.is_null());

        // Idempotent — second call should be no-op
        init_assertions().expect("init_assertions second call failed");

        cleanup_assertions();

        // Should be null after cleanup
        let ptr = context::get_assertion_table_ptr();
        assert!(ptr.is_null());
    }

    #[test]
    fn test_backward_compat_no_adaptive() {
        let config = ExplorationConfig {
            max_depth: 2,
            timelines_per_split: 4,
            global_energy: 100,
            adaptive: None,
            parallelism: None,
        };
        init(config).expect("init failed");

        // Energy budget pointer should be null (old path)
        let has_energy = context::ENERGY_BUDGET_PTR.with(|c| !c.get().is_null());
        assert!(!has_energy);

        cleanup();
    }
}
