//! Cross-process exploration statistics.
//!
//! All statistics live in `MAP_SHARED` memory so they are visible
//! across `fork()` boundaries. Atomic operations use `Relaxed` ordering
//! because the fork tree is sequential (parent waits on each child).

use std::io;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

use crate::shared_mem;

/// Global exploration statistics in shared memory.
#[repr(C)]
pub struct SharedStats {
    /// Remaining global energy budget (decremented per fork).
    pub global_energy: AtomicI64,
    /// Total number of timelines explored.
    pub total_timelines: AtomicU64,
    /// Total number of fork points triggered.
    pub fork_points: AtomicU64,
    /// Number of bugs found (child exited with code 42).
    pub bug_found: AtomicU64,
}

/// Maximum number of recipe entries.
pub const MAX_RECIPE_ENTRIES: usize = 128;

/// Shared recipe storage for the bug-finding timeline.
///
/// When a child exits with code 42 (bug found), the parent copies
/// the child's recipe here for later replay.
#[repr(C)]
pub struct SharedRecipe {
    /// Whether this recipe has been claimed (0 = no, 1 = yes).
    pub claimed: AtomicU32,
    /// Number of valid entries in [`Self::entries`].
    pub len: u32,
    /// Recipe entries: `(rng_call_count, child_seed)` pairs.
    pub entries: [(u64, u64); MAX_RECIPE_ENTRIES],
}

/// Snapshot of exploration statistics for test assertions.
#[derive(Debug, Clone)]
pub struct ExplorationStats {
    /// Remaining global energy.
    pub global_energy: i64,
    /// Total timelines explored.
    pub total_timelines: u64,
    /// Total fork points triggered.
    pub fork_points: u64,
    /// Number of bugs found.
    pub bug_found: u64,
    /// Energy remaining in the reallocation pool (0 when adaptive is disabled).
    pub realloc_pool_remaining: i64,
    /// Total instrumented code edges (from LLVM sancov). 0 when sancov unavailable.
    pub sancov_edges_total: usize,
    /// Code edges covered across all timelines. 0 when sancov unavailable.
    pub sancov_edges_covered: usize,
}

/// Allocate and initialize shared stats.
///
/// # Errors
///
/// Returns an error if shared memory allocation fails.
pub fn init_shared_stats(energy: i64) -> Result<*mut SharedStats, io::Error> {
    let ptr = shared_mem::alloc_shared(std::mem::size_of::<SharedStats>())?;
    let stats = ptr as *mut SharedStats;
    // Safety: ptr is valid, properly aligned (mmap returns page-aligned memory),
    // and zeroed. We initialize the energy field.
    unsafe {
        (*stats).global_energy.store(energy, Ordering::Relaxed);
    }
    Ok(stats)
}

/// Allocate and initialize shared recipe storage.
///
/// # Errors
///
/// Returns an error if shared memory allocation fails.
pub fn init_shared_recipe() -> Result<*mut SharedRecipe, io::Error> {
    let ptr = shared_mem::alloc_shared(std::mem::size_of::<SharedRecipe>())?;
    Ok(ptr as *mut SharedRecipe)
}

/// Try to consume one unit of energy. Returns `true` if successful.
///
/// # Safety
///
/// `stats` must point to valid shared stats allocated by [`init_shared_stats`].
pub unsafe fn decrement_energy(stats: *mut SharedStats) -> bool {
    unsafe { (*stats).global_energy.fetch_sub(1, Ordering::Relaxed) > 0 }
}

/// Reset shared stats for a new seed in multi-seed exploration.
///
/// Zeros counters and sets the display energy value.
///
/// # Safety
///
/// `stats` must point to valid shared memory allocated by [`init_shared_stats`].
pub unsafe fn reset_shared_stats(stats: *mut SharedStats, new_energy: i64) {
    let s = unsafe { &*stats };
    s.global_energy.store(new_energy, Ordering::Relaxed);
    s.total_timelines.store(0, Ordering::Relaxed);
    s.fork_points.store(0, Ordering::Relaxed);
    s.bug_found.store(0, Ordering::Relaxed);
}

/// Reset shared recipe for a new seed in multi-seed exploration.
///
/// Clears the claimed flag so the next seed can capture a new bug recipe.
///
/// # Safety
///
/// `recipe` must point to valid shared memory allocated by [`init_shared_recipe`].
pub unsafe fn reset_shared_recipe(recipe: *mut SharedRecipe) {
    let r = unsafe { &*recipe };
    r.claimed.store(0, Ordering::Relaxed);
}

/// Get a snapshot of the current exploration statistics.
///
/// Returns `None` if the stats pointer is null (exploration not initialized).
pub fn get_exploration_stats() -> Option<ExplorationStats> {
    let ptr = crate::context::SHARED_STATS.with(|c| c.get());
    if ptr.is_null() {
        return None;
    }

    // Read realloc pool from energy budget if available
    let realloc_pool = crate::context::ENERGY_BUDGET_PTR.with(|c| {
        let energy_ptr = c.get();
        if energy_ptr.is_null() {
            0
        } else {
            // Safety: energy_ptr was set during init
            unsafe { (*energy_ptr).realloc_pool.load(Ordering::Relaxed) }
        }
    });

    // Safety: ptr was set during init and points to valid shared stats
    unsafe {
        Some(ExplorationStats {
            global_energy: (*ptr).global_energy.load(Ordering::Relaxed),
            total_timelines: (*ptr).total_timelines.load(Ordering::Relaxed),
            fork_points: (*ptr).fork_points.load(Ordering::Relaxed),
            bug_found: (*ptr).bug_found.load(Ordering::Relaxed),
            realloc_pool_remaining: realloc_pool,
            sancov_edges_total: crate::sancov::sancov_edge_count(),
            sancov_edges_covered: crate::sancov::sancov_edges_covered(),
        })
    }
}

/// Get the bug recipe if one was captured.
///
/// Returns `None` if no bug was found or exploration is not initialized.
pub fn get_bug_recipe() -> Option<Vec<(u64, u64)>> {
    let ptr = crate::context::SHARED_RECIPE.with(|c| c.get());
    if ptr.is_null() {
        return None;
    }
    // Safety: ptr was set during init
    unsafe {
        let recipe = &*ptr;
        if recipe.claimed.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let len = recipe.len as usize;
        Some(recipe.entries[..len].to_vec())
    }
}
