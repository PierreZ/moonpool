//! Assertion slot tracking for fork-on-discovery decisions.
//!
//! Maintains a fixed-size table of assertion slots in shared memory.
//! When `sometimes_assert!` succeeds, [`maybe_fork_on_assertion`] checks
//! whether this assertion has triggered a fork before. If not, it triggers
//! [`crate::fork_loop::branch_on_discovery`] to explore alternate timelines.

/// Maximum number of tracked assertion slots.
pub const MAX_ASSERTION_SLOTS: usize = 128;

/// A single assertion tracking slot in shared memory.
///
/// All fields are accessed via raw pointer arithmetic on `MAP_SHARED` memory.
/// The sequential fork tree (parent waits on children) means no true atomic
/// CAS is needed — simple read-then-write suffices.
#[repr(C)]
pub struct AssertionSlot {
    /// FNV-1a hash of the assertion name.
    pub name_hash: u64,
    /// Whether a fork has been triggered for this assertion (0 = no, 1 = yes).
    pub fork_triggered: u8,
    /// Padding for alignment.
    pub _pad: [u8; 7],
    /// Total number of times this assertion passed.
    pub pass_count: u64,
    /// Total number of times this assertion failed.
    pub fail_count: u64,
}

/// FNV-1a hash for assertion name mapping.
fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Check if an assertion success should trigger forking.
///
/// Called from `sometimes_assert!` when the condition is true.
/// On first success for a given assertion name, triggers
/// [`crate::fork_loop::branch_on_discovery`] to fork child processes
/// with different RNG seeds.
///
/// This is a no-op if exploration is not active.
pub fn maybe_fork_on_assertion(name: &str) {
    if !crate::context::explorer_is_active() {
        return;
    }

    let table_ptr = crate::context::ASSERTION_TABLE.with(|c| c.get());
    if table_ptr.is_null() {
        return;
    }

    let hash = fnv1a(name.as_bytes());

    // Find existing slot or allocate a new one.
    // Safety: table_ptr points to MAX_ASSERTION_SLOTS slots of shared memory.
    unsafe {
        let mut empty_slot: Option<usize> = None;

        for i in 0..MAX_ASSERTION_SLOTS {
            let slot = &mut *table_ptr.add(i);

            if slot.name_hash == hash {
                // Found existing slot — increment pass count
                slot.pass_count += 1;

                // If not yet fork-triggered, trigger now
                if slot.fork_triggered == 0 {
                    slot.fork_triggered = 1;
                    // Mark this assertion in the coverage bitmap
                    let bm_ptr = crate::context::COVERAGE_BITMAP_PTR.with(|c| c.get());
                    if !bm_ptr.is_null() {
                        let bm = crate::coverage::CoverageBitmap::new(bm_ptr);
                        bm.set_bit(hash as usize);
                    }
                    crate::fork_loop::branch_on_discovery(name);
                }
                return;
            }

            if slot.name_hash == 0 && empty_slot.is_none() {
                empty_slot = Some(i);
            }
        }

        // Allocate a new slot if available
        if let Some(idx) = empty_slot {
            let slot = &mut *table_ptr.add(idx);
            slot.name_hash = hash;
            slot.pass_count = 1;
            slot.fork_triggered = 1;

            // Mark coverage and trigger fork
            let bm_ptr = crate::context::COVERAGE_BITMAP_PTR.with(|c| c.get());
            if !bm_ptr.is_null() {
                let bm = crate::coverage::CoverageBitmap::new(bm_ptr);
                bm.set_bit(hash as usize);
            }
            crate::fork_loop::branch_on_discovery(name);
        }
        // Table full — silently ignore (bounded resource)
    }
}

/// Record an assertion failure in the slot table.
///
/// Called to track assertion failures for statistics. Does not trigger forking.
pub fn record_assertion_failure(name: &str) {
    if !crate::context::explorer_is_active() {
        return;
    }

    let table_ptr = crate::context::ASSERTION_TABLE.with(|c| c.get());
    if table_ptr.is_null() {
        return;
    }

    let hash = fnv1a(name.as_bytes());

    // Safety: table_ptr points to MAX_ASSERTION_SLOTS slots of shared memory.
    unsafe {
        for i in 0..MAX_ASSERTION_SLOTS {
            let slot = &mut *table_ptr.add(i);
            if slot.name_hash == hash {
                slot.fail_count += 1;
                return;
            }
            if slot.name_hash == 0 {
                // Allocate new slot for tracking
                slot.name_hash = hash;
                slot.fail_count = 1;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fnv1a_deterministic() {
        let h1 = fnv1a(b"test_assertion");
        let h2 = fnv1a(b"test_assertion");
        assert_eq!(h1, h2);

        let h3 = fnv1a(b"different_assertion");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_fnv1a_no_collision_basic() {
        // Verify a few common names don't collide
        let names = ["a", "b", "c", "timeout", "connect", "retry"];
        let hashes: Vec<u64> = names.iter().map(|n| fnv1a(n.as_bytes())).collect();
        for i in 0..hashes.len() {
            for j in (i + 1)..hashes.len() {
                assert_ne!(
                    hashes[i], hashes[j],
                    "{} and {} collide",
                    names[i], names[j]
                );
            }
        }
    }
}
