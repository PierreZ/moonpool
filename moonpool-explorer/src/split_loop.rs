//! Timeline splitting loop.
//!
//! When a new assertion success is discovered, [`split_on_discovery`] forks
//! child processes with different seeds to explore alternate timelines from
//! that splitpoint forward.
//!
//! # Process Model
//!
//! ```text
//! Parent timeline (seed S0, depth D)
//!   |-- Timeline 0 (seed S0', depth D+1) -> waitpid -> merge coverage
//!   |-- Timeline 1 (seed S1', depth D+1) -> waitpid -> merge coverage
//!   |-- ...
//!   `-- Timeline N (seed SN', depth D+1) -> waitpid -> merge coverage
//!   resume parent timeline
//! ```
//!
//! Each child returns from this function and continues the simulation with
//! reseeded randomness. The parent waits for each child sequentially.
//!
//! # Platform notes
//!
//! This is an AFL-style fork server: the child returns from `fork()` and keeps
//! running the simulation in-process — it never calls `exec()`. On Linux that
//! is routine. On macOS, Apple officially considers `fork()`-without-`exec()`
//! unsupported: a child can abort if it re-enters Objective-C, libdispatch, or
//! Core Foundation state that another thread left mid-update across the fork
//! (the `__THE_PROCESS_HAS_FORKED_AND_YOU_CANNOT_USE_THIS_COREFOUNDATION_...`
//! crash).
//!
//! What makes the model safe here is that the simulation runtime is
//! **single-threaded** (`tokio::runtime::Builder::new_current_thread()`): there
//! are no sibling threads holding locks at fork time, and a pure-Rust child
//! does not touch the Apple frameworks that trip the fork-safety checks. The
//! single-thread precondition is therefore load-bearing, not incidental — see
//! the guard at the entry of [`split_on_discovery`]. Operators who do pull in
//! framework code can fall back to setting
//! `OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES` in the environment (it is read at
//! process start, so it cannot be set from inside this crate).

use std::sync::atomic::Ordering;

#[cfg(unix)]
use std::collections::HashMap;

use crate::context::{
    self, COVERAGE_BITMAP_PTR, ENERGY_BUDGET_PTR, EXPLORED_MAP_PTR, SHARED_RECIPE, SHARED_STATS,
};
#[cfg(unix)]
use crate::context::{BITMAP_POOL, BITMAP_POOL_SLOTS};
use crate::coverage::{COVERAGE_MAP_SIZE, CoverageBitmap, ExploredMap};
use crate::shared_stats::MAX_RECIPE_ENTRIES;

/// Compute a child seed by mixing the parent seed, assertion name, and child index.
///
/// Uses FNV-1a mixing to produce well-distributed seeds.
fn compute_child_seed(parent_seed: u64, mark_name: &str, child_idx: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in mark_name.as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash ^= parent_seed;
    hash = hash.wrapping_mul(0x0100_0000_01b3);
    hash ^= u64::from(child_idx);
    hash = hash.wrapping_mul(0x0100_0000_01b3);
    hash
}

/// Debug-only guard that the explorer is always driven from one thread.
///
/// The fork-without-exec model (see module docs) is only sound while the sim
/// runtime is single-threaded — that is the precondition that makes it safe on
/// macOS in particular. This records the first thread to reach a split point
/// and asserts every later split happens on that same thread, catching
/// accidental multi-threaded driving in debug builds. It compiles to nothing in
/// release, so it never touches fork throughput.
#[cfg(unix)]
fn assert_single_fork_thread() {
    use std::sync::OnceLock;
    use std::thread::ThreadId;

    static FORK_THREAD: OnceLock<ThreadId> = OnceLock::new();
    let current = std::thread::current().id();
    let first = *FORK_THREAD.get_or_init(|| current);
    debug_assert_eq!(
        first, current,
        "explorer fork loop entered from a second thread; the fork-without-exec \
         model requires the single-threaded sim runtime (see module docs)"
    );
}

/// Controls how many children can run in parallel during splitting.
///
/// When set on [`crate::ExplorationConfig::parallelism`], the fork loop
/// uses a sliding window of this many concurrent children instead of the
/// default sequential fork→wait→fork→wait cycle.
#[derive(Debug, Clone)]
pub enum Parallelism {
    /// Use all available CPU cores (`sysconf(_SC_NPROCESSORS_ONLN)`).
    MaxCores,
    /// Use half the available CPU cores.
    HalfCores,
    /// Use exactly this many concurrent children.
    Cores(usize),
    /// Use all available cores minus `n` (e.g., leave 1 for the OS).
    MaxCoresMinus(usize),
}

/// Resolve a [`Parallelism`] value to a concrete slot count (≥ 1).
#[cfg(unix)]
fn resolve_parallelism(p: &Parallelism) -> usize {
    // Safety: sysconf reads a system configuration value and does not
    // dereference any pointers. It is always safe to call.
    let ncpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    let ncpus = if ncpus > 0 {
        usize::try_from(ncpus).unwrap_or(1)
    } else {
        1
    };
    let n = match p {
        Parallelism::MaxCores => ncpus,
        Parallelism::HalfCores => ncpus / 2,
        Parallelism::Cores(c) => *c,
        Parallelism::MaxCoresMinus(minus) => ncpus.saturating_sub(*minus),
    };
    n.max(1) // always at least 1
}

/// Get or initialize the per-process bitmap pool in shared memory.
///
/// Returns the pool base pointer, or null if allocation fails.
/// Each forked child resets this to null so it allocates its own pool
/// if it becomes a parent (avoids sharing pool slots with siblings).
#[cfg(unix)]
fn get_or_init_pool(slot_count: usize) -> *mut u8 {
    let existing = BITMAP_POOL.with(std::cell::Cell::get);
    let existing_slots = BITMAP_POOL_SLOTS.with(std::cell::Cell::get);

    if !existing.is_null() && existing_slots >= slot_count {
        return existing;
    }

    // Free old pool if it exists but is too small
    if !existing.is_null() {
        // Safety: ptr was returned by alloc_shared with existing_slots * COVERAGE_MAP_SIZE
        unsafe {
            crate::shared_mem::free_shared(existing, existing_slots * COVERAGE_MAP_SIZE);
        }
        BITMAP_POOL.with(|c| c.set(std::ptr::null_mut()));
        BITMAP_POOL_SLOTS.with(|c| c.set(0));
    }

    match crate::shared_mem::alloc_shared(slot_count * COVERAGE_MAP_SIZE) {
        Ok(ptr) => {
            BITMAP_POOL.with(|c| c.set(ptr));
            BITMAP_POOL_SLOTS.with(|c| c.set(slot_count));
            ptr
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Return the pointer to slot `idx` within a bitmap pool.
#[cfg(unix)]
fn pool_slot(pool_base: *mut u8, idx: usize) -> *mut u8 {
    // Safety: caller ensures idx < slot_count and pool_base is valid
    unsafe { pool_base.add(idx * COVERAGE_MAP_SIZE) }
}

/// Common child-process setup after fork: reseed RNG, update context, bump counter.
///
/// Also resets the bitmap pool pointer so nested splits allocate a fresh pool.
#[cfg(unix)]
fn setup_child(
    child_seed: u64,
    split_call_count: u64,
    stats_ptr: *mut crate::shared_stats::SharedStats,
) {
    context::rng_reseed(child_seed);
    context::with_ctx_mut(|ctx| {
        ctx.is_child = true;
        ctx.depth += 1;
        ctx.current_seed = child_seed;
        ctx.recipe.push((split_call_count, child_seed));
    });
    if !stats_ptr.is_null() {
        // Safety: stats_ptr points to valid shared memory
        unsafe {
            (*stats_ptr).total_timelines.fetch_add(1, Ordering::Relaxed);
        }
    }
    // Reset bitmap pool so nested splits allocate a fresh pool
    BITMAP_POOL.with(|c| c.set(std::ptr::null_mut()));
    BITMAP_POOL_SLOTS.with(|c| c.set(0));

    // Zero BSS counters so child captures only its OWN sancov edges
    crate::sancov::reset_bss_counters();
    // Reset sancov pool so nested splits allocate a fresh pool
    crate::sancov::SANCOV_POOL.with(|c| c.set(std::ptr::null_mut()));
    crate::sancov::SANCOV_POOL_SLOTS.with(|c| c.set(0));
}

/// Shared state passed to fork/reap helpers — bundles the cross-process
/// pointers (vm/stats/pool) along with the parent's RNG split point.
#[cfg(unix)]
struct ForkSharedState {
    /// Snapshot of the RNG call count at the split point.
    split_call_count: u64,
    /// Explored-map pointer (cumulative coverage across timelines).
    vm_ptr: *mut u8,
    /// Shared statistics pointer.
    stats_ptr: *mut crate::shared_stats::SharedStats,
    /// Bitmap pool base pointer (per-slot per-child coverage scratch).
    pool_base: *mut u8,
    /// Sancov pool base pointer (or null).
    sancov_pool_base: *mut u8,
}

/// Parallel-fork state resolved once per split call.
#[cfg(unix)]
struct ParallelState {
    /// Number of concurrent child slots (0 → sequential mode).
    slot_count: usize,
    /// Base pointer of the per-slot bitmap pool.
    pool_base: *mut u8,
    /// Base pointer of the per-slot sancov pool (or null when sancov is unused).
    sancov_pool_base: *mut u8,
    /// Parent's sancov transfer pointer, restored after splitting.
    parent_sancov_transfer: *mut u8,
    /// True when concurrent execution is enabled.
    parallel: bool,
}

/// Resolve parallelism configuration and allocate per-slot pools.
#[cfg(unix)]
fn resolve_parallel_state() -> ParallelState {
    let parallelism = context::with_ctx(|ctx| ctx.parallelism.clone());
    let (slot_count, pool_base) = if let Some(ref p) = parallelism {
        let sc = resolve_parallelism(p);
        let pb = get_or_init_pool(sc);
        if pb.is_null() {
            (0, std::ptr::null_mut())
        } else {
            (sc, pb)
        }
    } else {
        (0, std::ptr::null_mut())
    };
    let parallel = slot_count > 0;

    let sancov_pool_base = if parallel {
        crate::sancov::get_or_init_sancov_pool(slot_count)
    } else {
        std::ptr::null_mut()
    };
    let parent_sancov_transfer = if parallel && !sancov_pool_base.is_null() {
        crate::sancov::SANCOV_TRANSFER.with(std::cell::Cell::get)
    } else {
        std::ptr::null_mut()
    };

    ParallelState {
        slot_count,
        pool_base,
        sancov_pool_base,
        parent_sancov_transfer,
        parallel,
    }
}

/// Drain all in-flight children and merge their coverage into the parent.
#[cfg(unix)]
fn drain_active_children(
    active: &mut HashMap<libc::pid_t, (u64, usize)>,
    free_slots: &mut Vec<usize>,
    shared: &ForkSharedState,
    batch_has_new: &mut bool,
) {
    while !active.is_empty() {
        reap_one(active, free_slots, shared, batch_has_new);
    }
}

/// Restore parent's bitmap and sancov state after the split loop completes.
#[cfg(unix)]
fn restore_parent_state(state: &ParallelState, bm_ptr: *mut u8, parent_bitmap_backup: &[u8]) {
    if state.parallel {
        // Restore parent's bitmap pointer (children used pool slots).
        COVERAGE_BITMAP_PTR.with(|c| c.set(bm_ptr));
        if !state.sancov_pool_base.is_null() {
            crate::sancov::SANCOV_TRANSFER.with(|c| c.set(state.parent_sancov_transfer));
        }
    } else if !bm_ptr.is_null() {
        // Sequential mode: restore parent bitmap content from backup.
        // Safety: bm_ptr points to COVERAGE_MAP_SIZE bytes, backup is also COVERAGE_MAP_SIZE.
        unsafe {
            std::ptr::copy_nonoverlapping(parent_bitmap_backup.as_ptr(), bm_ptr, COVERAGE_MAP_SIZE);
        }
    }
}

/// Outcome of attempting to spawn a child timeline.
#[cfg(unix)]
enum SpawnOutcome {
    /// Spawn succeeded; caller should continue the loop.
    Continued,
    /// Fork failed or back-pressure unavailable; caller should break the loop.
    Stop,
    /// We are now the child process; caller must return immediately.
    InChild,
}

/// Spawn one parallel child: reserve a pool slot, fork, and on success insert
/// into the active map. Returns the [`SpawnOutcome`] for the caller's loop.
#[cfg(unix)]
fn spawn_parallel_child(
    child_seed: u64,
    shared: &ForkSharedState,
    active: &mut HashMap<libc::pid_t, (u64, usize)>,
    free_slots: &mut Vec<usize>,
    batch_has_new: &mut bool,
) -> SpawnOutcome {
    while free_slots.is_empty() {
        reap_one(active, free_slots, shared, batch_has_new);
    }
    let Some(slot) = free_slots.pop() else {
        return SpawnOutcome::Stop;
    };
    let slot_ptr = pool_slot(shared.pool_base, slot);

    // Safety: slot_ptr is valid shared memory of COVERAGE_MAP_SIZE bytes.
    unsafe {
        std::ptr::write_bytes(slot_ptr, 0, COVERAGE_MAP_SIZE);
    }
    COVERAGE_BITMAP_PTR.with(|c| c.set(slot_ptr));

    if !shared.sancov_pool_base.is_null() {
        let sancov_len = crate::sancov::sancov_edge_count();
        // Safety: sancov_slot is within sancov_pool_base for sancov_len bytes.
        unsafe {
            let sancov_slot = crate::sancov::sancov_pool_slot(shared.sancov_pool_base, slot);
            std::ptr::write_bytes(sancov_slot, 0, sancov_len);
            crate::sancov::SANCOV_TRANSFER.with(|c| c.set(sancov_slot));
        }
    }

    // Safety: single-threaded, no real I/O
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => {
            free_slots.push(slot);
            SpawnOutcome::Stop
        }
        0 => {
            setup_child(child_seed, shared.split_call_count, shared.stats_ptr);
            SpawnOutcome::InChild
        }
        child_pid => {
            active.insert(child_pid, (child_seed, slot));
            SpawnOutcome::Continued
        }
    }
}

/// Spawn one sequential child: clear parent bitmap, fork, wait, merge coverage.
///
/// `track_new_bits` controls whether we update `batch_has_new` on coverage
/// growth (used by the adaptive batch-yield check).
#[cfg(unix)]
fn spawn_sequential_child(
    child_seed: u64,
    bm_ptr: *mut u8,
    shared: &ForkSharedState,
    batch_has_new: &mut bool,
    track_new_bits: bool,
) -> SpawnOutcome {
    if !bm_ptr.is_null() {
        // Safety: bm_ptr points to COVERAGE_MAP_SIZE bytes.
        let bm = unsafe { CoverageBitmap::new(bm_ptr) };
        bm.clear();
    }
    crate::sancov::clear_transfer_buffer();

    // Safety: single-threaded, no real I/O
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => SpawnOutcome::Stop,
        0 => {
            setup_child(child_seed, shared.split_call_count, shared.stats_ptr);
            SpawnOutcome::InChild
        }
        child_pid => {
            let mut status: libc::c_int = 0;
            // Safety: child_pid is a valid child PID we just forked.
            unsafe { libc::waitpid(child_pid, &raw mut status, 0) };

            if !bm_ptr.is_null() && !shared.vm_ptr.is_null() {
                // Safety: both pointers are valid for COVERAGE_MAP_SIZE bytes.
                let bm = unsafe { CoverageBitmap::new(bm_ptr) };
                let vm = unsafe { ExploredMap::new(shared.vm_ptr) };
                if track_new_bits && vm.has_new_bits(&bm) {
                    *batch_has_new = true;
                }
                vm.merge_from(&bm);
            }
            *batch_has_new |= crate::sancov::has_new_sancov_coverage();

            if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 42 {
                if !shared.stats_ptr.is_null() {
                    // Safety: stats_ptr is valid shared memory.
                    unsafe {
                        (*shared.stats_ptr)
                            .bug_found
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                save_bug_recipe(shared.split_call_count, child_seed);
            }

            if !shared.stats_ptr.is_null() {
                // Safety: stats_ptr is valid shared memory.
                unsafe {
                    (*shared.stats_ptr)
                        .fork_points
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            SpawnOutcome::Continued
        }
    }
}

/// Reap one finished child via `waitpid(-1)`, merge its coverage, check for bugs.
///
/// Removes the reaped PID from `active`, pushes its slot back to `free_slots`,
/// and sets `batch_has_new` if the child contributed new coverage bits.
#[cfg(unix)]
fn reap_one(
    active: &mut HashMap<libc::pid_t, (u64, usize)>,
    free_slots: &mut Vec<usize>,
    shared: &ForkSharedState,
    batch_has_new: &mut bool,
) {
    let mut status: libc::c_int = 0;
    // Safety: waitpid(-1) waits for any child of this process
    let finished_pid = unsafe { libc::waitpid(-1, &raw mut status, 0) };
    if finished_pid <= 0 {
        return;
    }

    let Some((child_seed, slot)) = active.remove(&finished_pid) else {
        return;
    };

    // Merge child's coverage bitmap into explored map
    if !shared.vm_ptr.is_null() {
        // Safety: pool_base + slot offset is valid shared memory
        let child_bm = unsafe { CoverageBitmap::new(pool_slot(shared.pool_base, slot)) };
        let vm = unsafe { ExploredMap::new(shared.vm_ptr) };
        if vm.has_new_bits(&child_bm) {
            *batch_has_new = true;
        }
        vm.merge_from(&child_bm);
    }

    // Check child's sancov coverage from its pool slot
    if !shared.sancov_pool_base.is_null() {
        let sancov_slot = unsafe { crate::sancov::sancov_pool_slot(shared.sancov_pool_base, slot) };
        if crate::sancov::has_new_sancov_coverage_from(sancov_slot) {
            *batch_has_new = true;
        }
    }

    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 42 {
        if !shared.stats_ptr.is_null() {
            // Safety: stats_ptr is valid shared memory.
            unsafe {
                (*shared.stats_ptr)
                    .bug_found
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        save_bug_recipe(shared.split_call_count, child_seed);
    }

    if !shared.stats_ptr.is_null() {
        // Safety: stats_ptr is valid shared memory.
        unsafe {
            (*shared.stats_ptr)
                .fork_points
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    free_slots.push(slot);
}

/// Configuration for adaptive batch-based timeline splitting.
///
/// Instead of spawning a fixed number of timelines, the adaptive loop
/// spawns in batches and checks coverage yield between batches. Productive
/// marks (that find new coverage) get more timelines; barren marks stop
/// early and return their energy to the reallocation pool.
#[derive(Debug, Clone)]
pub struct AdaptiveConfig {
    /// Number of children to fork per batch before checking coverage yield.
    pub batch_size: u32,
    /// Minimum total forks for a mark (even if barren after first batch).
    pub min_timelines: u32,
    /// Maximum total forks for a mark (hard cap).
    pub max_timelines: u32,
    /// Initial per-mark energy budget.
    pub per_mark_energy: i64,
    /// Minimum timelines for marks during warm starts (explored map has prior
    /// coverage from previous seeds). Defaults to `batch_size` if `None`.
    pub warm_min_timelines: Option<u32>,
}

/// Dispatch to either adaptive or fixed-count splitting based on config.
///
/// If an energy budget is configured (adaptive mode), uses coverage-yield-driven
/// batching. Otherwise falls back to the fixed `timelines_per_split` behavior.
#[cfg(unix)]
pub(crate) fn dispatch_split(mark_name: &str, slot_idx: usize) {
    let has_adaptive = ENERGY_BUDGET_PTR.with(|c| !c.get().is_null());
    if has_adaptive {
        adaptive_split_on_discovery(mark_name, slot_idx);
    } else {
        split_on_discovery(mark_name);
    }
}

/// No-op on non-unix platforms.
#[cfg(not(unix))]
pub(crate) fn dispatch_split(_mark_name: &str, _slot_idx: usize) {}

/// Read adaptive batch sizing from the explorer context.
///
/// Returns `(batch_size, max_timelines, effective_min_timelines)` where the
/// minimum-timelines value already accounts for the warm-start override.
#[cfg(unix)]
fn read_adaptive_batch_config() -> (u32, u32, u32) {
    context::with_ctx(|ctx| {
        let (batch_size, min_timelines, max_timelines) =
            ctx.adaptive.as_ref().map_or((4, 1, 16), |a| {
                (a.batch_size, a.min_timelines, a.max_timelines)
            });
        let warm_min = ctx
            .adaptive
            .as_ref()
            .and_then(|a| a.warm_min_timelines)
            .unwrap_or(batch_size);
        let effective_min = if ctx.warm_start {
            warm_min
        } else {
            min_timelines
        };
        (batch_size, max_timelines, effective_min)
    })
}

/// Adaptive split: spawn timelines in batches, check coverage yield, stop when barren.
///
/// When parallelism is configured, uses a sliding window of concurrent children
/// capped at the resolved slot count. Otherwise falls back to sequential forking.
#[cfg(unix)]
fn adaptive_split_on_discovery(mark_name: &str, slot_idx: usize) {
    assert_single_fork_thread();
    // Read context for guard checks
    let (ctx_active, depth, max_depth, current_seed) =
        context::with_ctx(|ctx| (ctx.active, ctx.depth, ctx.max_depth, ctx.current_seed));

    if !ctx_active || depth >= max_depth {
        return;
    }

    let budget_ptr = ENERGY_BUDGET_PTR.with(std::cell::Cell::get);
    if budget_ptr.is_null() {
        return;
    }

    // Initialize per-mark budget on first use
    // Safety: budget_ptr is valid shared memory
    unsafe {
        crate::energy::init_mark_budget(budget_ptr, slot_idx);
    }

    let split_call_count = context::rng_get_count();

    let bm_ptr = COVERAGE_BITMAP_PTR.with(std::cell::Cell::get);
    let vm_ptr = EXPLORED_MAP_PTR.with(std::cell::Cell::get);
    let stats_ptr = SHARED_STATS.with(std::cell::Cell::get);

    let (batch_size, max_timelines, effective_min_timelines) = read_adaptive_batch_config();

    let state = resolve_parallel_state();
    let shared = ForkSharedState {
        split_call_count,
        vm_ptr,
        stats_ptr,
        pool_base: state.pool_base,
        sancov_pool_base: state.sancov_pool_base,
    };

    // Save parent bitmap (sequential only — parallel children use pool slots)
    let mut parent_bitmap_backup = [0u8; COVERAGE_MAP_SIZE];
    if !state.parallel && !bm_ptr.is_null() {
        // Safety: bm_ptr points to COVERAGE_MAP_SIZE bytes
        unsafe {
            std::ptr::copy_nonoverlapping(
                bm_ptr,
                parent_bitmap_backup.as_mut_ptr(),
                COVERAGE_MAP_SIZE,
            );
        }
    }

    let mut timelines_spawned: u32 = 0;

    // Parallel state (only used when parallel == true)
    let mut active: HashMap<libc::pid_t, (u64, usize)> = HashMap::new();
    let mut free_slots: Vec<usize> = if state.parallel {
        (0..state.slot_count).collect()
    } else {
        Vec::new()
    };

    // Batch loop
    loop {
        let mut batch_has_new = false;
        let batch_start = timelines_spawned;

        while timelines_spawned - batch_start < batch_size {
            if timelines_spawned >= max_timelines {
                break;
            }

            // Safety: budget_ptr is valid
            if !unsafe { crate::energy::decrement_mark_energy(budget_ptr, slot_idx) } {
                break;
            }

            let child_seed = compute_child_seed(current_seed, mark_name, timelines_spawned);
            timelines_spawned += 1;

            let outcome = if state.parallel {
                spawn_parallel_child(
                    child_seed,
                    &shared,
                    &mut active,
                    &mut free_slots,
                    &mut batch_has_new,
                )
            } else {
                spawn_sequential_child(child_seed, bm_ptr, &shared, &mut batch_has_new, true)
            };
            match outcome {
                SpawnOutcome::Continued => {}
                SpawnOutcome::Stop => break,
                SpawnOutcome::InChild => return,
            }
        }

        drain_active_children(&mut active, &mut free_slots, &shared, &mut batch_has_new);

        // Batch complete — decide whether to continue.
        if timelines_spawned >= max_timelines {
            break;
        }
        if !batch_has_new && timelines_spawned >= effective_min_timelines {
            // Barren — return remaining energy to pool.
            // Safety: budget_ptr is valid
            unsafe {
                crate::energy::return_mark_energy_to_pool(budget_ptr, slot_idx);
            }
            break;
        }
        // Energy ran out mid-batch.
        if timelines_spawned - batch_start < batch_size && timelines_spawned < max_timelines {
            break;
        }
    }

    restore_parent_state(&state, bm_ptr, &parent_bitmap_backup);
}

/// Split the simulation timeline at a discovery point.
///
/// Called when an assertion detects a new success (e.g. via `assertion_bool`
/// or `assertion_numeric`). Spawns `timelines_per_split` child timelines,
/// each with a different seed derived from the current seed and the mark name.
///
/// When parallelism is configured, uses a sliding window of concurrent children.
/// Otherwise falls back to sequential fork→wait→fork→wait.
///
/// Must be called from the single-threaded sim runtime: the fork-without-exec
/// model relies on there being no sibling threads at fork time (see module-level
/// `# Platform notes`). A debug guard enforces this.
#[cfg(unix)]
pub fn split_on_discovery(mark_name: &str) {
    assert_single_fork_thread();
    let (ctx_active, depth, max_depth, timelines_per_split, current_seed) =
        context::with_ctx(|ctx| {
            (
                ctx.active,
                ctx.depth,
                ctx.max_depth,
                ctx.timelines_per_split,
                ctx.current_seed,
            )
        });

    if !ctx_active || depth >= max_depth {
        return;
    }

    let stats_ptr = SHARED_STATS.with(std::cell::Cell::get);
    if stats_ptr.is_null() {
        return;
    }
    // Safety: stats_ptr set during init, points to valid shared stats
    if !unsafe { crate::shared_stats::decrement_energy(stats_ptr) } {
        return;
    }

    let split_call_count = context::rng_get_count();
    let bm_ptr = COVERAGE_BITMAP_PTR.with(std::cell::Cell::get);
    let vm_ptr = EXPLORED_MAP_PTR.with(std::cell::Cell::get);

    let state = resolve_parallel_state();
    let shared = ForkSharedState {
        split_call_count,
        vm_ptr,
        stats_ptr,
        pool_base: state.pool_base,
        sancov_pool_base: state.sancov_pool_base,
    };

    // Save parent bitmap (sequential only)
    let mut parent_bitmap_backup = [0u8; COVERAGE_MAP_SIZE];
    if !state.parallel && !bm_ptr.is_null() {
        // Safety: bm_ptr points to COVERAGE_MAP_SIZE bytes
        unsafe {
            std::ptr::copy_nonoverlapping(
                bm_ptr,
                parent_bitmap_backup.as_mut_ptr(),
                COVERAGE_MAP_SIZE,
            );
        }
    }

    // Parallel state
    let mut active: HashMap<libc::pid_t, (u64, usize)> = HashMap::new();
    let mut free_slots: Vec<usize> = if state.parallel {
        (0..state.slot_count).collect()
    } else {
        Vec::new()
    };
    let mut batch_has_new = false;

    for child_idx in 0..timelines_per_split {
        if child_idx > 0 {
            // Safety: stats_ptr is valid
            if !unsafe { crate::shared_stats::decrement_energy(stats_ptr) } {
                break;
            }
        }

        let child_seed = compute_child_seed(current_seed, mark_name, child_idx);

        let outcome = if state.parallel {
            spawn_parallel_child(
                child_seed,
                &shared,
                &mut active,
                &mut free_slots,
                &mut batch_has_new,
            )
        } else {
            spawn_sequential_child(child_seed, bm_ptr, &shared, &mut batch_has_new, false)
        };
        match outcome {
            SpawnOutcome::Continued => {}
            SpawnOutcome::Stop => break,
            SpawnOutcome::InChild => return,
        }
    }

    drain_active_children(&mut active, &mut free_slots, &shared, &mut batch_has_new);

    restore_parent_state(&state, bm_ptr, &parent_bitmap_backup);
}

/// No-op on non-unix platforms.
#[cfg(not(unix))]
pub fn split_on_discovery(_mark_name: &str) {}

/// Save a bug recipe to shared memory.
fn save_bug_recipe(split_call_count: u64, child_seed: u64) {
    let recipe_ptr = SHARED_RECIPE.with(std::cell::Cell::get);
    if recipe_ptr.is_null() {
        return;
    }

    // Safety: recipe_ptr points to valid shared memory
    unsafe {
        let recipe = &mut *recipe_ptr;

        // Only save the first bug recipe (CAS from 0 to 1)
        if recipe
            .claimed
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            // Copy the current context's recipe plus this fork point
            context::with_ctx(|ctx| {
                let total_entries = ctx.recipe.len() + 1;
                let len = total_entries.min(MAX_RECIPE_ENTRIES);

                // Copy existing recipe entries
                for (i, &entry) in ctx.recipe.iter().take(len - 1).enumerate() {
                    recipe.entries[i] = entry;
                }
                // Add the current fork point
                if len > 0 {
                    recipe.entries[len - 1] = (split_call_count, child_seed);
                }
                recipe.len = u32::try_from(len).expect("len bounded by MAX_RECIPE_ENTRIES");
            });
        }
    }
}

/// Exit the current child process with the given code.
///
/// Calls `libc::_exit()` which skips atexit handlers and stdio flushing.
/// This is appropriate for forked child processes.
///
/// # Safety
///
/// This function terminates the process immediately. Only call from a
/// forked child process.
#[cfg(unix)]
pub fn exit_child(code: i32) -> ! {
    crate::sancov::copy_counters_to_shared();
    // Safety: _exit is always safe to call; it terminates the process.
    unsafe { libc::_exit(code) }
}

/// Panics on non-unix platforms (should never be called).
#[cfg(not(unix))]
pub fn exit_child(code: i32) -> ! {
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_child_seed_deterministic() {
        let s1 = compute_child_seed(42, "test", 0);
        let s2 = compute_child_seed(42, "test", 0);
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_compute_child_seed_varies_by_index() {
        let s0 = compute_child_seed(42, "test", 0);
        let s1 = compute_child_seed(42, "test", 1);
        let s2 = compute_child_seed(42, "test", 2);
        assert_ne!(s0, s1);
        assert_ne!(s1, s2);
        assert_ne!(s0, s2);
    }

    #[test]
    fn test_compute_child_seed_varies_by_name() {
        let s1 = compute_child_seed(42, "alpha", 0);
        let s2 = compute_child_seed(42, "beta", 0);
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_compute_child_seed_varies_by_parent() {
        let s1 = compute_child_seed(1, "test", 0);
        let s2 = compute_child_seed(2, "test", 0);
        assert_ne!(s1, s2);
    }
}
