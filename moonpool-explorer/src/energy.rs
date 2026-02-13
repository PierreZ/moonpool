//! 3-level energy budget for adaptive exploration.
//!
//! Provides global, per-mark, and reallocation pool energy levels in
//! `MAP_SHARED` memory. When an assertion mark is barren (no new coverage),
//! its remaining per-mark energy is returned to the reallocation pool.
//! Productive marks that exhaust their budget can draw from the pool.
//!
//! All atomics use `Relaxed` ordering because the fork tree is sequential
//! (parent waits on each child).

use std::io;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::assertion_slots::MAX_ASSERTION_SLOTS;
use crate::shared_mem;

/// 3-level energy budget in shared memory.
///
/// Lives in `MAP_SHARED` memory so all forked processes share the same counters.
#[repr(C)]
pub struct EnergyBudget {
    /// Global energy remaining across all marks.
    pub global_remaining: AtomicI64,
    /// Per-mark energy budgets (indexed by assertion slot index).
    pub per_mark: [AtomicI64; MAX_ASSERTION_SLOTS],
    /// Initial energy assigned to each new mark.
    pub per_mark_initial: i64,
    /// Pool of energy returned by barren marks, available for productive marks.
    pub realloc_pool: AtomicI64,
}

/// Allocate and initialize an energy budget in shared memory.
///
/// # Errors
///
/// Returns an error if shared memory allocation fails.
pub fn init_energy_budget(
    global_energy: i64,
    per_mark_initial: i64,
) -> Result<*mut EnergyBudget, io::Error> {
    let ptr = shared_mem::alloc_shared(std::mem::size_of::<EnergyBudget>())?;
    let budget = ptr as *mut EnergyBudget;
    // Safety: ptr is valid, zeroed by mmap. Initialize non-zero fields.
    unsafe {
        (*budget)
            .global_remaining
            .store(global_energy, Ordering::Relaxed);
        (*budget).per_mark_initial = per_mark_initial;
        // per_mark and realloc_pool start at 0 (zeroed by mmap).
        // Per-mark budgets are lazily initialized via init_mark_budget().
    }
    Ok(budget)
}

/// Initialize a mark's per-mark energy budget (called on first fork for a slot).
///
/// # Safety
///
/// `budget` must point to a valid `EnergyBudget` in shared memory.
pub unsafe fn init_mark_budget(budget: *mut EnergyBudget, slot_idx: usize) {
    if slot_idx < MAX_ASSERTION_SLOTS {
        let b = unsafe { &*budget };
        b.per_mark[slot_idx].store(b.per_mark_initial, Ordering::Relaxed);
    }
}

/// Try to consume one unit of energy for a mark. Returns `true` if successful.
///
/// Attempts in order: global budget, per-mark budget, then reallocation pool.
/// Undoes partial decrements on failure to maintain consistency.
///
/// # Safety
///
/// `budget` must point to a valid `EnergyBudget` in shared memory.
pub unsafe fn decrement_mark_energy(budget: *mut EnergyBudget, slot_idx: usize) -> bool {
    let b = unsafe { &*budget };

    // Check global budget first
    if b.global_remaining.fetch_sub(1, Ordering::Relaxed) <= 0 {
        b.global_remaining.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    // Try per-mark budget
    if slot_idx < MAX_ASSERTION_SLOTS {
        if b.per_mark[slot_idx].fetch_sub(1, Ordering::Relaxed) > 0 {
            return true;
        }
        // Per-mark exhausted, undo and try realloc pool
        b.per_mark[slot_idx].fetch_add(1, Ordering::Relaxed);

        if b.realloc_pool.fetch_sub(1, Ordering::Relaxed) > 0 {
            return true;
        }
        b.realloc_pool.fetch_add(1, Ordering::Relaxed);
    }

    // Neither per-mark nor realloc pool had energy — undo global
    b.global_remaining.fetch_add(1, Ordering::Relaxed);
    false
}

/// Return a mark's remaining per-mark energy to the reallocation pool.
///
/// Called when a mark is determined to be barren (no new coverage found).
///
/// # Safety
///
/// `budget` must point to a valid `EnergyBudget` in shared memory.
pub unsafe fn return_mark_energy_to_pool(budget: *mut EnergyBudget, slot_idx: usize) {
    if slot_idx < MAX_ASSERTION_SLOTS {
        let b = unsafe { &*budget };
        let remaining = b.per_mark[slot_idx].swap(0, Ordering::Relaxed);
        if remaining > 0 {
            b.realloc_pool.fetch_add(remaining, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mark_budget_decrement_and_return() {
        let ptr = init_energy_budget(100, 10).expect("init failed");
        unsafe {
            init_mark_budget(ptr, 0);
            // Consume 2 from per_mark[0]
            assert!(decrement_mark_energy(ptr, 0));
            assert!(decrement_mark_energy(ptr, 0));

            // Return remaining (8) to pool
            return_mark_energy_to_pool(ptr, 0);
            let b = &*ptr;
            assert_eq!(b.per_mark[0].load(Ordering::Relaxed), 0);
            assert_eq!(b.realloc_pool.load(Ordering::Relaxed), 8);

            shared_mem::free_shared(ptr as *mut u8, std::mem::size_of::<EnergyBudget>());
        }
    }

    #[test]
    fn test_productive_mark_draws_from_realloc() {
        let ptr = init_energy_budget(100, 5).expect("init failed");
        unsafe {
            // Mark 0: barren, returns energy to pool
            init_mark_budget(ptr, 0);
            decrement_mark_energy(ptr, 0); // consume 1
            return_mark_energy_to_pool(ptr, 0); // return 4 to pool

            // Mark 1: productive, exhausts per_mark budget
            init_mark_budget(ptr, 1);
            for _ in 0..5 {
                assert!(decrement_mark_energy(ptr, 1));
            }
            // Per-mark exhausted, draws from realloc pool (which has 4)
            assert!(decrement_mark_energy(ptr, 1));
            let b = &*ptr;
            assert_eq!(b.realloc_pool.load(Ordering::Relaxed), 3);

            shared_mem::free_shared(ptr as *mut u8, std::mem::size_of::<EnergyBudget>());
        }
    }

    #[test]
    fn test_global_energy_exhaustion() {
        let ptr = init_energy_budget(3, 100).expect("init failed");
        unsafe {
            init_mark_budget(ptr, 0);
            assert!(decrement_mark_energy(ptr, 0));
            assert!(decrement_mark_energy(ptr, 0));
            assert!(decrement_mark_energy(ptr, 0));
            // Global exhausted — 4th fails
            assert!(!decrement_mark_energy(ptr, 0));
            // Global should be back to 0 (not negative)
            let b = &*ptr;
            assert_eq!(b.global_remaining.load(Ordering::Relaxed), 0);

            shared_mem::free_shared(ptr as *mut u8, std::mem::size_of::<EnergyBudget>());
        }
    }

    #[test]
    fn test_realloc_flow() {
        let ptr = init_energy_budget(50, 5).expect("init failed");
        unsafe {
            // Mark 0: barren, consume 1, return 4 to pool
            init_mark_budget(ptr, 0);
            decrement_mark_energy(ptr, 0);
            return_mark_energy_to_pool(ptr, 0);

            let b = &*ptr;
            assert_eq!(b.realloc_pool.load(Ordering::Relaxed), 4);

            // Mark 1: productive, exhausts budget then draws 3 from pool
            init_mark_budget(ptr, 1);
            for _ in 0..5 {
                assert!(decrement_mark_energy(ptr, 1));
            }
            for _ in 0..3 {
                assert!(decrement_mark_energy(ptr, 1));
            }
            assert_eq!(b.realloc_pool.load(Ordering::Relaxed), 1);

            shared_mem::free_shared(ptr as *mut u8, std::mem::size_of::<EnergyBudget>());
        }
    }
}
