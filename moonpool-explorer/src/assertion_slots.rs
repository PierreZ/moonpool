//! Rich assertion slot tracking for the Antithesis-style assertion suite.
//!
//! Maintains a fixed-size table of assertion slots in shared memory.
//! Supports boolean assertions (always/sometimes/reachable/unreachable),
//! numeric guidance assertions (with watermark tracking), and compound
//! boolean assertions (sometimes-all with frontier tracking).
//!
//! Each slot is accessed via raw pointer arithmetic on `MAP_SHARED` memory.
//! The sequential fork tree (parent waits on children) means no true atomic
//! CAS races occur — simple CAS suffices for correctness across forks.

use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, Ordering};

/// Maximum number of tracked assertion slots.
pub const MAX_ASSERTION_SLOTS: usize = 128;

/// Maximum length of the assertion message stored in a slot.
const SLOT_MSG_LEN: usize = 64;

/// Total size of the assertion table memory region in bytes.
///
/// Layout: `[next_slot: u32, _pad: u32, slots: [AssertionSlot; MAX_ASSERTION_SLOTS]]`
pub const ASSERTION_TABLE_MEM_SIZE: usize =
    8 + MAX_ASSERTION_SLOTS * std::mem::size_of::<AssertionSlot>();

/// The kind of assertion being tracked.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertKind {
    /// Invariant that must always hold when reached.
    Always = 0,
    /// Invariant that must hold when reached, but need not be reached.
    AlwaysOrUnreachable = 1,
    /// Condition that should sometimes be true.
    Sometimes = 2,
    /// Code path that should be reached at least once.
    Reachable = 3,
    /// Code path that should never be reached.
    Unreachable = 4,
    /// Numeric invariant that must always hold (e.g., val > threshold).
    NumericAlways = 5,
    /// Numeric condition that should sometimes hold.
    NumericSometimes = 6,
    /// Compound boolean: all named bools should sometimes be true simultaneously.
    BooleanSometimesAll = 7,
}

impl AssertKind {
    /// Convert from raw u8 to AssertKind, returning None for invalid values.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Always),
            1 => Some(Self::AlwaysOrUnreachable),
            2 => Some(Self::Sometimes),
            3 => Some(Self::Reachable),
            4 => Some(Self::Unreachable),
            5 => Some(Self::NumericAlways),
            6 => Some(Self::NumericSometimes),
            7 => Some(Self::BooleanSometimesAll),
            _ => None,
        }
    }
}

/// Comparison operator for numeric assertions.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertCmp {
    /// Greater than.
    Gt = 0,
    /// Greater than or equal to.
    Ge = 1,
    /// Less than.
    Lt = 2,
    /// Less than or equal to.
    Le = 3,
}

/// A single assertion tracking slot in shared memory.
///
/// All fields are accessed via raw pointer arithmetic on `MAP_SHARED` memory.
#[repr(C)]
pub struct AssertionSlot {
    /// FNV-1a hash of the assertion message (u32).
    pub msg_hash: u32,
    /// The kind of assertion (AssertKind as u8).
    pub kind: u8,
    /// Whether this assertion must be hit (1) or not (0).
    pub must_hit: u8,
    /// Whether to maximize (1) or minimize (0) the watermark value.
    pub maximize: u8,
    /// Whether a fork has been triggered for this assertion (0 = no, 1 = yes).
    pub fork_triggered: u8,
    /// Total number of times this assertion passed.
    pub pass_count: u64,
    /// Total number of times this assertion failed.
    pub fail_count: u64,
    /// Numeric watermark: best value observed (for guidance assertions).
    pub watermark: i64,
    /// Watermark value at last fork (for detecting improvement).
    pub fork_watermark: i64,
    /// Frontier: number of simultaneously true bools (for BooleanSometimesAll).
    pub frontier: u8,
    /// Padding for alignment.
    pub _pad: [u8; 7],
    /// Assertion message string (null-terminated).
    pub msg: [u8; SLOT_MSG_LEN],
}

impl AssertionSlot {
    /// Get the assertion message as a string slice.
    pub fn msg_str(&self) -> &str {
        let len = self
            .msg
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(SLOT_MSG_LEN);
        std::str::from_utf8(&self.msg[..len]).unwrap_or("???")
    }
}

/// FNV-1a hash of a message string to a stable u32.
pub fn msg_hash(msg: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in msg.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Find an existing slot or allocate a new one by msg_hash.
///
/// Returns a pointer to the slot and its index, or null if the table is full.
///
/// # Safety
///
/// `table_ptr` must point to a valid assertion table region of at least
/// `ASSERTION_TABLE_MEM_SIZE` bytes.
unsafe fn find_or_alloc_slot(
    table_ptr: *mut u8,
    hash: u32,
    kind: AssertKind,
    must_hit: u8,
    maximize: u8,
    msg: &str,
) -> (*mut AssertionSlot, usize) {
    unsafe {
        let next_atomic = &*(table_ptr as *const AtomicU32);
        let count = next_atomic.load(Ordering::Relaxed) as usize;
        let base = table_ptr.add(8) as *mut AssertionSlot;

        // Search existing slots.
        for i in 0..count.min(MAX_ASSERTION_SLOTS) {
            let slot = base.add(i);
            if (*slot).msg_hash == hash {
                return (slot, i);
            }
        }

        // Allocate new slot atomically.
        let new_idx = next_atomic.fetch_add(1, Ordering::Relaxed) as usize;
        if new_idx >= MAX_ASSERTION_SLOTS {
            next_atomic.fetch_sub(1, Ordering::Relaxed);
            return (std::ptr::null_mut(), 0);
        }

        let slot = base.add(new_idx);
        let mut msg_buf = [0u8; SLOT_MSG_LEN];
        let n = msg.len().min(SLOT_MSG_LEN - 1);
        msg_buf[..n].copy_from_slice(&msg.as_bytes()[..n]);

        std::ptr::write(
            slot,
            AssertionSlot {
                msg_hash: hash,
                kind: kind as u8,
                must_hit,
                maximize,
                fork_triggered: 0,
                pass_count: 0,
                fail_count: 0,
                watermark: if maximize == 1 { i64::MIN } else { i64::MAX },
                fork_watermark: if maximize == 1 { i64::MIN } else { i64::MAX },
                frontier: 0,
                _pad: [0; 7],
                msg: msg_buf,
            },
        );
        (slot, new_idx)
    }
}

/// Trigger forking for a slot that discovered something new.
///
/// Writes to coverage bitmap and virgin map (if pointers are non-null),
/// then calls `dispatch_branch()` if exploration is active.
fn assertion_branch(slot_idx: usize, hash: u32) {
    // Mark coverage bitmap
    let bm_ptr = crate::context::COVERAGE_BITMAP_PTR.with(|c| c.get());
    if !bm_ptr.is_null() {
        let bm = unsafe { crate::coverage::CoverageBitmap::new(bm_ptr) };
        bm.set_bit(hash as usize);
    }

    // Mark virgin map
    let vm_ptr = crate::context::VIRGIN_MAP_PTR.with(|c| c.get());
    if !vm_ptr.is_null() {
        let vm = unsafe { crate::coverage::VirginMap::new(vm_ptr) };
        let bm_ptr2 = crate::context::COVERAGE_BITMAP_PTR.with(|c| c.get());
        if !bm_ptr2.is_null() {
            let bm = unsafe { crate::coverage::CoverageBitmap::new(bm_ptr2) };
            vm.merge_from(&bm);
        }
    }

    // Dispatch to fork loop if explorer is active
    if crate::context::explorer_is_active() {
        crate::fork_loop::dispatch_branch("", slot_idx % MAX_ASSERTION_SLOTS);
    }
}

/// Boolean assertion backing function.
///
/// Handles Always, AlwaysOrUnreachable, Sometimes, Reachable, and Unreachable.
/// Gets or allocates a slot, increments pass/fail counts, and triggers forking
/// for Sometimes/Reachable assertions on first success.
///
/// This is a no-op if the assertion table is not initialized.
pub fn assertion_bool(kind: AssertKind, must_hit: bool, condition: bool, msg: &str) {
    let table_ptr = crate::context::get_assertion_table_ptr();
    if table_ptr.is_null() {
        return;
    }

    let hash = msg_hash(msg);
    let must_hit_u8 = if must_hit { 1 } else { 0 };

    // Safety: table_ptr points to ASSERTION_TABLE_MEM_SIZE bytes of shared memory.
    let (slot, slot_idx) =
        unsafe { find_or_alloc_slot(table_ptr, hash, kind, must_hit_u8, 0, msg) };
    if slot.is_null() {
        return;
    }

    // Safety: slot points to valid shared memory.
    unsafe {
        match kind {
            AssertKind::Always | AssertKind::AlwaysOrUnreachable | AssertKind::NumericAlways => {
                if condition {
                    let pc = &*((&(*slot).pass_count) as *const u64 as *const AtomicI64);
                    pc.fetch_add(1, Ordering::Relaxed);
                } else {
                    let fc = &*((&(*slot).fail_count) as *const u64 as *const AtomicI64);
                    let prev = fc.fetch_add(1, Ordering::Relaxed);
                    if prev == 0 {
                        eprintln!("[ASSERTION FAILED] {} (kind={:?})", msg, kind);
                    }
                }
            }
            AssertKind::Sometimes | AssertKind::Reachable => {
                if condition {
                    let pc = &*((&(*slot).pass_count) as *const u64 as *const AtomicI64);
                    pc.fetch_add(1, Ordering::Relaxed);

                    // CAS fork_triggered from 0 → 1 on first success
                    let ft = &*((&(*slot).fork_triggered) as *const u8 as *const AtomicU8);
                    if ft
                        .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        assertion_branch(slot_idx, hash);
                    }
                } else {
                    let fc = &*((&(*slot).fail_count) as *const u64 as *const AtomicI64);
                    fc.fetch_add(1, Ordering::Relaxed);
                }
            }
            AssertKind::Unreachable => {
                // Being reached at all is a "pass" (the assertion is that we should NOT reach)
                // We track it as pass_count = times reached (bad), fail_count unused
                let pc = &*((&(*slot).pass_count) as *const u64 as *const AtomicI64);
                let prev = pc.fetch_add(1, Ordering::Relaxed);
                if prev == 0 {
                    eprintln!("[UNREACHABLE REACHED] {}", msg);
                }
            }
            _ => {}
        }
    }
}

/// Numeric guidance assertion backing function.
///
/// Evaluates a comparison (left `cmp` right), tracks pass/fail counts,
/// and maintains a watermark of the best observed value of `left`.
/// For NumericSometimes, forks when the watermark improves past the
/// last fork watermark.
///
/// `maximize` determines whether improving means getting larger (true) or smaller (false).
///
/// This is a no-op if the assertion table is not initialized.
pub fn assertion_numeric(
    kind: AssertKind,
    cmp: AssertCmp,
    maximize: bool,
    left: i64,
    right: i64,
    msg: &str,
) {
    let table_ptr = crate::context::get_assertion_table_ptr();
    if table_ptr.is_null() {
        return;
    }

    let hash = msg_hash(msg);
    let maximize_u8 = if maximize { 1 } else { 0 };

    // Safety: table_ptr points to ASSERTION_TABLE_MEM_SIZE bytes.
    let (slot, slot_idx) =
        unsafe { find_or_alloc_slot(table_ptr, hash, kind, 1, maximize_u8, msg) };
    if slot.is_null() {
        return;
    }

    // Evaluate the comparison
    let passes = match cmp {
        AssertCmp::Gt => left > right,
        AssertCmp::Ge => left >= right,
        AssertCmp::Lt => left < right,
        AssertCmp::Le => left <= right,
    };

    // Safety: slot points to valid shared memory.
    unsafe {
        if passes {
            let pc = &*((&(*slot).pass_count) as *const u64 as *const AtomicI64);
            pc.fetch_add(1, Ordering::Relaxed);
        } else {
            let fc = &*((&(*slot).fail_count) as *const u64 as *const AtomicI64);
            let prev = fc.fetch_add(1, Ordering::Relaxed);
            if kind == AssertKind::NumericAlways && prev == 0 {
                eprintln!(
                    "[NUMERIC ASSERTION FAILED] {} (left={}, right={}, cmp={:?})",
                    msg, left, right, cmp
                );
            }
        }

        // Update watermark: track best value of `left`
        let wm = &*((&(*slot).watermark) as *const i64 as *const AtomicI64);
        let mut current = wm.load(Ordering::Relaxed);
        loop {
            let is_better = if maximize {
                left > current
            } else {
                left < current
            };
            if !is_better {
                break;
            }
            match wm.compare_exchange_weak(current, left, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }

        // For NumericSometimes: fork when watermark improves past fork_watermark
        if kind == AssertKind::NumericSometimes {
            let fw = &*((&(*slot).fork_watermark) as *const i64 as *const AtomicI64);
            let mut fork_current = fw.load(Ordering::Relaxed);
            loop {
                let is_better = if maximize {
                    left > fork_current
                } else {
                    left < fork_current
                };
                if !is_better {
                    break;
                }
                match fw.compare_exchange_weak(
                    fork_current,
                    left,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        assertion_branch(slot_idx, hash);
                        break;
                    }
                    Err(actual) => fork_current = actual,
                }
            }
        }
    }
}

/// Compound boolean assertion backing function (sometimes-all).
///
/// Counts how many of the named booleans are simultaneously true.
/// Maintains a frontier (max count seen). Forks when the frontier advances.
///
/// This is a no-op if the assertion table is not initialized.
pub fn assertion_sometimes_all(msg: &str, named_bools: &[(&str, bool)]) {
    let table_ptr = crate::context::get_assertion_table_ptr();
    if table_ptr.is_null() {
        return;
    }

    let hash = msg_hash(msg);

    // Safety: table_ptr points to ASSERTION_TABLE_MEM_SIZE bytes.
    let (slot, slot_idx) =
        unsafe { find_or_alloc_slot(table_ptr, hash, AssertKind::BooleanSometimesAll, 1, 0, msg) };
    if slot.is_null() {
        return;
    }

    // Count simultaneously true bools
    let true_count = named_bools.iter().filter(|(_, v)| *v).count() as u8;

    // Safety: slot points to valid shared memory.
    unsafe {
        // Increment pass_count (always, for statistics)
        let pc = &*((&(*slot).pass_count) as *const u64 as *const AtomicI64);
        pc.fetch_add(1, Ordering::Relaxed);

        // CAS loop on frontier — fork when it advances
        let fr = &*((&(*slot).frontier) as *const u8 as *const AtomicU8);
        let mut current = fr.load(Ordering::Relaxed);
        loop {
            if true_count <= current {
                break;
            }
            match fr.compare_exchange_weak(
                current,
                true_count,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    assertion_branch(slot_idx, hash);
                    break;
                }
                Err(actual) => current = actual,
            }
        }
    }
}

/// Read all allocated assertion slots from shared memory.
///
/// Returns an empty vector if the assertion table is not initialized.
pub fn assertion_read_all() -> Vec<AssertionSlotSnapshot> {
    let table_ptr = crate::context::get_assertion_table_ptr();
    if table_ptr.is_null() {
        return Vec::new();
    }

    unsafe {
        let count = (*(table_ptr as *const u32)) as usize;
        let count = count.min(MAX_ASSERTION_SLOTS);
        let base = table_ptr.add(8) as *const AssertionSlot;

        (0..count)
            .map(|i| {
                let slot = &*base.add(i);
                AssertionSlotSnapshot {
                    msg: slot.msg_str().to_string(),
                    kind: slot.kind,
                    must_hit: slot.must_hit,
                    pass_count: slot.pass_count,
                    fail_count: slot.fail_count,
                    watermark: slot.watermark,
                    frontier: slot.frontier,
                }
            })
            .collect()
    }
}

/// A snapshot of an assertion slot for reporting.
#[derive(Debug, Clone)]
pub struct AssertionSlotSnapshot {
    /// The assertion message.
    pub msg: String,
    /// The kind of assertion (AssertKind as u8).
    pub kind: u8,
    /// Whether this assertion must be hit.
    pub must_hit: u8,
    /// Number of times the assertion passed.
    pub pass_count: u64,
    /// Number of times the assertion failed.
    pub fail_count: u64,
    /// Best watermark value (for numeric assertions).
    pub watermark: i64,
    /// Frontier value (for BooleanSometimesAll).
    pub frontier: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_msg_hash_deterministic() {
        let h1 = msg_hash("test_assertion");
        let h2 = msg_hash("test_assertion");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_msg_hash_no_collision() {
        let names = ["a", "b", "c", "timeout", "connect", "retry"];
        let hashes: Vec<u32> = names.iter().map(|n| msg_hash(n)).collect();
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

    #[test]
    fn test_slot_size_stable() {
        // Verify AssertionSlot size for shared memory layout stability.
        // msg_hash(4) + kind(1) + must_hit(1) + maximize(1) + fork_triggered(1) +
        // pass_count(8) + fail_count(8) + watermark(8) + fork_watermark(8) +
        // frontier(1) + _pad(7) + msg(64) = 112
        assert_eq!(std::mem::size_of::<AssertionSlot>(), 112);
    }

    #[test]
    fn test_assertion_bool_noop_when_inactive() {
        // Should not panic when assertion table is not initialized.
        assertion_bool(AssertKind::Sometimes, true, true, "test");
        assertion_bool(AssertKind::Always, true, false, "test2");
    }

    #[test]
    fn test_assertion_numeric_noop_when_inactive() {
        // Should not panic when assertion table is not initialized.
        assertion_numeric(
            AssertKind::NumericAlways,
            AssertCmp::Gt,
            false,
            10,
            5,
            "test",
        );
    }

    #[test]
    fn test_assertion_read_all_when_inactive() {
        // Should return empty when not initialized.
        let slots = assertion_read_all();
        assert!(slots.is_empty());
    }

    #[test]
    fn test_assert_kind_from_u8() {
        assert_eq!(AssertKind::from_u8(0), Some(AssertKind::Always));
        assert_eq!(
            AssertKind::from_u8(7),
            Some(AssertKind::BooleanSometimesAll)
        );
        assert_eq!(AssertKind::from_u8(8), None);
    }
}
