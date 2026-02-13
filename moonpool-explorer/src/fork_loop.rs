//! Fork-based exploration loop.
//!
//! When a new assertion success is discovered, [`branch_on_discovery`] forks
//! child processes with different seeds to explore alternate timelines from
//! that point forward.
//!
//! # Process Model
//!
//! ```text
//! Parent (seed S0, depth D)
//!   |-- fork child 0 (seed S0', depth D+1) -> waitpid -> merge coverage
//!   |-- fork child 1 (seed S1', depth D+1) -> waitpid -> merge coverage
//!   |-- ...
//!   `-- fork child N (seed SN', depth D+1) -> waitpid -> merge coverage
//!   resume parent execution
//! ```
//!
//! Each child returns from this function and continues the simulation with
//! reseeded randomness. The parent waits for each child sequentially.

use std::sync::atomic::Ordering;

use crate::context::{self, COVERAGE_BITMAP_PTR, SHARED_RECIPE, SHARED_STATS, VIRGIN_MAP_PTR};
use crate::coverage::{COVERAGE_MAP_SIZE, CoverageBitmap, VirginMap};
use crate::shared_stats::MAX_RECIPE_ENTRIES;

/// Compute a child seed by mixing the parent seed, assertion name, and child index.
///
/// Uses FNV-1a mixing to produce well-distributed seeds.
fn compute_child_seed(parent_seed: u64, mark_name: &str, child_idx: u32) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in mark_name.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash ^= parent_seed;
    hash = hash.wrapping_mul(0x100000001b3);
    hash ^= child_idx as u64;
    hash = hash.wrapping_mul(0x100000001b3);
    hash
}

/// Branch the simulation timeline at a discovery point.
///
/// Called when [`crate::assertion_slots::maybe_fork_on_assertion`] detects a
/// new assertion success. Forks `children_per_fork` child processes, each with
/// a different seed derived from the current seed and the assertion name.
///
/// **In the child**: sets `is_child = true`, increments depth, reseeds the RNG,
/// and returns (the simulation continues with new randomness).
///
/// **In the parent**: calls `waitpid()` for each child, merges coverage, checks
/// exit codes, and continues after all children are done.
#[cfg(unix)]
pub fn branch_on_discovery(mark_name: &str) {
    // Read context for guard checks
    let (active, depth, max_depth, children_per_fork, current_seed) = context::with_ctx(|ctx| {
        (
            ctx.active,
            ctx.depth,
            ctx.max_depth,
            ctx.children_per_fork,
            ctx.current_seed,
        )
    });

    if !active || depth >= max_depth {
        return;
    }

    // Check energy budget
    let stats_ptr = SHARED_STATS.with(|c| c.get());
    if stats_ptr.is_null() {
        return;
    }
    // Safety: stats_ptr set during init, points to valid shared stats
    if !unsafe { crate::shared_stats::decrement_energy(stats_ptr) } {
        return;
    }

    // Record the current RNG call count for the recipe
    let fork_call_count = context::rng_get_count();

    // Get shared memory pointers
    let bm_ptr = COVERAGE_BITMAP_PTR.with(|c| c.get());
    let vm_ptr = VIRGIN_MAP_PTR.with(|c| c.get());

    // Save parent bitmap (copy to stack)
    let mut parent_bitmap_backup = [0u8; COVERAGE_MAP_SIZE];
    if !bm_ptr.is_null() {
        // Safety: bm_ptr points to COVERAGE_MAP_SIZE bytes
        unsafe {
            std::ptr::copy_nonoverlapping(
                bm_ptr,
                parent_bitmap_backup.as_mut_ptr(),
                COVERAGE_MAP_SIZE,
            );
        }
    }

    // Fork children
    for child_idx in 0..children_per_fork {
        // Additional children beyond the first each consume energy
        if child_idx > 0 {
            // Safety: stats_ptr is valid
            if !unsafe { crate::shared_stats::decrement_energy(stats_ptr) } {
                break;
            }
        }

        // Clear child bitmap before fork
        if !bm_ptr.is_null() {
            // Safety: bm_ptr is valid
            let bm = unsafe { CoverageBitmap::new(bm_ptr) };
            bm.clear();
        }

        let child_seed = compute_child_seed(current_seed, mark_name, child_idx);

        // Safety: moonpool-sim is single-threaded, no real I/O, no file descriptors,
        // no mutexes. After fork, child has COW copy of address space.
        // MAP_SHARED memory is the only cross-process communication.
        let pid = unsafe { libc::fork() };

        match pid {
            -1 => {
                // Fork failed — continue without forking
                break;
            }
            0 => {
                // CHILD PROCESS
                // Reseed RNG via the hook (also resets call count)
                context::rng_reseed(child_seed);

                // Update context
                context::with_ctx_mut(|ctx| {
                    ctx.is_child = true;
                    ctx.depth += 1;
                    ctx.current_seed = child_seed;
                    ctx.recipe.push((fork_call_count, child_seed));
                });

                // Increment total timelines counter
                // Safety: stats_ptr is valid shared memory
                unsafe {
                    (*stats_ptr).total_timelines.fetch_add(1, Ordering::Relaxed);
                }

                // Return — child continues the simulation with new randomness.
                // It will eventually reach the end of orchestrate_workloads
                // where moonpool-sim calls exit_child().
                return;
            }
            child_pid => {
                // PARENT PROCESS — wait for child
                let mut status: libc::c_int = 0;
                // Safety: child_pid is a valid PID returned by fork()
                unsafe {
                    libc::waitpid(child_pid, &mut status, 0);
                }

                // Merge child coverage into virgin map
                if !bm_ptr.is_null() && !vm_ptr.is_null() {
                    // Safety: both pointers are valid shared memory
                    let bm = unsafe { CoverageBitmap::new(bm_ptr) };
                    let vm = unsafe { VirginMap::new(vm_ptr) };
                    vm.merge_from(&bm);
                }

                // Check if child found a bug (exit code 42)
                let exited_normally = libc::WIFEXITED(status);
                if exited_normally {
                    let exit_code = libc::WEXITSTATUS(status);
                    if exit_code == 42 {
                        // Safety: stats_ptr is valid
                        unsafe {
                            (*stats_ptr).bug_found.fetch_add(1, Ordering::Relaxed);
                        }
                        // Save the bug recipe
                        save_bug_recipe(fork_call_count, child_seed);
                    }
                }

                // Increment fork points counter
                // Safety: stats_ptr is valid
                unsafe {
                    (*stats_ptr).fork_points.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    // Restore parent bitmap
    if !bm_ptr.is_null() {
        // Safety: bm_ptr points to COVERAGE_MAP_SIZE bytes
        unsafe {
            std::ptr::copy_nonoverlapping(parent_bitmap_backup.as_ptr(), bm_ptr, COVERAGE_MAP_SIZE);
        }
    }
}

/// No-op on non-unix platforms.
#[cfg(not(unix))]
pub fn branch_on_discovery(_mark_name: &str) {}

/// Save a bug recipe to shared memory.
fn save_bug_recipe(fork_call_count: u64, child_seed: u64) {
    let recipe_ptr = SHARED_RECIPE.with(|c| c.get());
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
                    recipe.entries[len - 1] = (fork_call_count, child_seed);
                }
                recipe.len = len as u32;
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
