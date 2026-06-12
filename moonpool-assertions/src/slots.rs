//! Rich assertion slot tracking for the Antithesis-style assertion suite.
//!
//! Maintains a fixed-size table of assertion slots. Supports boolean assertions
//! (always/sometimes/reachable/unreachable), numeric guidance assertions (with
//! watermark tracking), and compound boolean assertions (sometimes-all with
//! frontier tracking).
//!
//! Each slot is accessed via raw pointer arithmetic on the assertion region
//! (heap by default, or `MAP_SHARED` memory when an exploration backend installs
//! one). With multiple fork children running concurrently, `find_or_alloc_slot`
//! claims slots by atomically writing `msg_hash` before re-scanning, ensuring
//! concurrent allocators see each other.
//!
//! On a "discovery" (first Sometimes/Reachable pass, numeric watermark
//! improvement, or frontier advance) the accounting calls
//! [`crate::hooks::on_slot_discovery`]. With no hook installed this is a no-op
//! (pure accounting); the exploration backend wires it to coverage + forking.

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
    /// Convert from raw u8 to `AssertKind`, returning None for invalid values.
    #[must_use]
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

/// A single assertion tracking slot.
///
/// All fields are accessed via raw pointer arithmetic on the assertion region.
#[repr(C)]
pub struct AssertionSlot {
    /// FNV-1a hash of the assertion message (u32).
    pub msg_hash: u32,
    /// The kind of assertion (`AssertKind` as u8).
    pub kind: u8,
    /// Whether this assertion must be hit (1) or not (0).
    pub must_hit: u8,
    /// Whether to maximize (1) or minimize (0) the watermark value.
    pub maximize: u8,
    /// Whether a fork has been triggered for this assertion (0 = no, 1 = yes).
    pub split_triggered: u8,
    /// Total number of times this assertion passed.
    pub pass_count: u64,
    /// Total number of times this assertion failed.
    pub fail_count: u64,
    /// Numeric watermark: best value observed (for guidance assertions).
    pub watermark: i64,
    /// Watermark value at last fork (for detecting improvement).
    pub split_watermark: i64,
    /// Frontier: number of simultaneously true bools (for `BooleanSometimesAll`).
    pub frontier: u8,
    /// Padding for alignment.
    pad: [u8; 7],
    /// Assertion message string (null-terminated).
    pub msg: [u8; SLOT_MSG_LEN],
}

impl AssertionSlot {
    /// Get the assertion message as a string slice.
    #[must_use]
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
#[must_use]
pub fn msg_hash(msg: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in msg.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Find an existing slot or allocate a new one by `msg_hash`.
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
        let next_atomic = &*table_ptr.cast::<()>().cast::<AtomicU32>();
        let count = next_atomic.load(Ordering::Acquire) as usize;
        let base = table_ptr.add(8).cast::<()>().cast::<AssertionSlot>();

        // Search existing slots (atomic load to see concurrent writers).
        for i in 0..count.min(MAX_ASSERTION_SLOTS) {
            let slot = base.add(i);
            let h = &*std::ptr::addr_of!((*slot).msg_hash).cast::<AtomicU32>();
            if h.load(Ordering::Acquire) == hash {
                return (slot, i);
            }
        }

        // Allocate new slot atomically.
        let new_idx = next_atomic.fetch_add(1, Ordering::AcqRel) as usize;
        if new_idx >= MAX_ASSERTION_SLOTS {
            next_atomic.fetch_sub(1, Ordering::AcqRel);
            return (std::ptr::null_mut(), 0);
        }

        // Claim our slot by writing msg_hash atomically BEFORE re-scanning.
        // This makes our claim visible to any concurrent process doing
        // its own re-scan, preventing the TOCTOU duplicate slot race.
        let slot = base.add(new_idx);
        let hash_atomic = &*std::ptr::addr_of!((*slot).msg_hash).cast::<AtomicU32>();
        hash_atomic.store(hash, Ordering::Release);

        // Re-scan 0..new_idx for a concurrent registration of the same hash.
        // Lower index always wins — if we find a match, tombstone ourselves.
        for i in 0..new_idx {
            let existing = base.add(i);
            let existing_hash = &*std::ptr::addr_of!((*existing).msg_hash).cast::<AtomicU32>();
            if existing_hash.load(Ordering::Acquire) == hash {
                // Another process claimed a lower slot. Tombstone ours.
                hash_atomic.store(0, Ordering::Release);
                std::ptr::write_bytes(slot.cast::<u8>(), 0, std::mem::size_of::<AssertionSlot>());
                return (existing, i);
            }
        }

        // No duplicate found — write remaining slot fields (msg_hash already set).
        let mut msg_buf = [0u8; SLOT_MSG_LEN];
        let n = msg.len().min(SLOT_MSG_LEN - 1);
        msg_buf[..n].copy_from_slice(&msg.as_bytes()[..n]);

        (*slot).kind = kind as u8;
        (*slot).must_hit = must_hit;
        (*slot).maximize = maximize;
        (*slot).split_triggered = 0;
        (*slot).pass_count = 0;
        (*slot).fail_count = 0;
        (*slot).watermark = if maximize == 1 { i64::MIN } else { i64::MAX };
        (*slot).split_watermark = if maximize == 1 { i64::MIN } else { i64::MAX };
        (*slot).frontier = 0;
        (*slot).pad = [0; 7];
        (*slot).msg = msg_buf;

        (slot, new_idx)
    }
}

/// Boolean assertion backing function.
///
/// Handles Always, `AlwaysOrUnreachable`, Sometimes, Reachable, and Unreachable.
/// Gets or allocates a slot, increments pass/fail counts, and signals a discovery
/// for Sometimes/Reachable assertions on first success.
///
/// This is a no-op if the assertion table is not initialized.
pub fn assertion_bool(kind: AssertKind, must_hit: bool, condition: bool, msg: &str) {
    let table_ptr = crate::region::assertion_table_ptr();
    if table_ptr.is_null() {
        return;
    }

    let hash = msg_hash(msg);
    let must_hit_u8 = u8::from(must_hit);

    // Safety: table_ptr points to ASSERTION_TABLE_MEM_SIZE bytes.
    let (slot, slot_idx) =
        unsafe { find_or_alloc_slot(table_ptr, hash, kind, must_hit_u8, 0, msg) };
    if slot.is_null() {
        return;
    }

    // Safety: slot points to valid memory.
    unsafe {
        match kind {
            AssertKind::Always | AssertKind::AlwaysOrUnreachable | AssertKind::NumericAlways => {
                if condition {
                    let pc = &*(&raw const (*slot).pass_count).cast::<AtomicI64>();
                    pc.fetch_add(1, Ordering::Relaxed);
                } else {
                    let fc = &*(&raw const (*slot).fail_count).cast::<AtomicI64>();
                    let prev = fc.fetch_add(1, Ordering::Relaxed);
                    if prev == 0 {
                        eprintln!("[ASSERTION FAILED] {msg} (kind={kind:?})");
                    }
                }
            }
            AssertKind::Sometimes | AssertKind::Reachable => {
                if condition {
                    let pc = &*(&raw const (*slot).pass_count).cast::<AtomicI64>();
                    pc.fetch_add(1, Ordering::Relaxed);

                    // CAS split_triggered from 0 → 1 on first success
                    let ft = &*(&raw const (*slot).split_triggered).cast::<AtomicU8>();
                    if ft
                        .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        crate::hooks::on_slot_discovery(slot_idx, hash);
                    }
                } else {
                    let fc = &*(&raw const (*slot).fail_count).cast::<AtomicI64>();
                    fc.fetch_add(1, Ordering::Relaxed);
                }
            }
            AssertKind::Unreachable => {
                // Being reached at all is a "pass" (the assertion is that we should NOT reach)
                // We track it as pass_count = times reached (bad), fail_count unused
                let pc = &*(&raw const (*slot).pass_count).cast::<AtomicI64>();
                let prev = pc.fetch_add(1, Ordering::Relaxed);
                if prev == 0 {
                    eprintln!("[UNREACHABLE REACHED] {msg}");
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
/// For `NumericSometimes`, signals a discovery when the watermark improves past
/// the last fork watermark.
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
    let table_ptr = crate::region::assertion_table_ptr();
    if table_ptr.is_null() {
        return;
    }

    let hash = msg_hash(msg);
    let maximize_u8 = u8::from(maximize);

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

    // Safety: slot points to valid memory.
    unsafe {
        if passes {
            let pc = &*(&raw const (*slot).pass_count).cast::<AtomicI64>();
            pc.fetch_add(1, Ordering::Relaxed);
        } else {
            let fc = &*(&raw const (*slot).fail_count).cast::<AtomicI64>();
            let prev = fc.fetch_add(1, Ordering::Relaxed);
            if kind == AssertKind::NumericAlways && prev == 0 {
                eprintln!(
                    "[NUMERIC ASSERTION FAILED] {msg} (left={left}, right={right}, cmp={cmp:?})"
                );
            }
        }

        // Update watermark: track best value of `left`
        let wm = &*(&raw const (*slot).watermark).cast::<AtomicI64>();
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

        // For NumericSometimes: signal discovery when watermark improves past split_watermark
        if kind == AssertKind::NumericSometimes {
            let fw = &*(&raw const (*slot).split_watermark).cast::<AtomicI64>();
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
                        crate::hooks::on_slot_discovery(slot_idx, hash);
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
/// Maintains a frontier (max count seen). Signals a discovery when the frontier
/// advances.
///
/// This is a no-op if the assertion table is not initialized.
pub fn assertion_sometimes_all(msg: &str, named_bools: &[(&str, bool)]) {
    let table_ptr = crate::region::assertion_table_ptr();
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

    // Count simultaneously true bools. The frontier field is u8, so we cap at u8::MAX —
    // callers passing more than 255 named bools is not a supported use case; clamp
    // via `unwrap_or(u8::MAX)` so we never panic.
    let true_count =
        u8::try_from(named_bools.iter().filter(|(_, v)| *v).count()).unwrap_or(u8::MAX);

    // Safety: slot points to valid memory.
    unsafe {
        // Increment pass_count (always, for statistics)
        let pc = &*(&raw const (*slot).pass_count).cast::<AtomicI64>();
        pc.fetch_add(1, Ordering::Relaxed);

        // CAS loop on frontier — signal discovery when it advances
        let fr = &*(&raw const (*slot).frontier).cast::<AtomicU8>();
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
                    crate::hooks::on_slot_discovery(slot_idx, hash);
                    break;
                }
                Err(actual) => current = actual,
            }
        }
    }
}

/// Read all allocated assertion slots from the region.
///
/// Returns an empty vector if the assertion table is not initialized.
#[must_use]
pub fn assertion_read_all() -> Vec<AssertionSlotSnapshot> {
    let table_ptr = crate::region::assertion_table_ptr();
    if table_ptr.is_null() {
        return Vec::new();
    }

    // Safety: table_ptr was allocated with ASSERTION_TABLE_MEM_SIZE bytes.
    // - The first 4 bytes hold the slot count (u32), capped at MAX_ASSERTION_SLOTS.
    // - base = table_ptr + 8 is the start of the AssertionSlot array.
    // - Loop bound 0..count ensures base.add(i) stays within the allocated region.
    // - AssertionSlot fields are read through a shared reference; tombstoned slots
    //   (msg_hash == 0) are skipped.
    unsafe {
        let count = (*table_ptr.cast::<()>().cast::<u32>()) as usize;
        let count = count.min(MAX_ASSERTION_SLOTS);
        let base = table_ptr.add(8).cast::<()>().cast::<AssertionSlot>();

        (0..count)
            .filter_map(|i| {
                let slot = &*base.add(i);
                // Skip tombstones (msg_hash == 0) left by the duplicate-slot race fix.
                if slot.msg_hash == 0 {
                    return None;
                }
                Some(AssertionSlotSnapshot {
                    msg: slot.msg_str().to_string(),
                    kind: slot.kind,
                    must_hit: slot.must_hit,
                    pass_count: slot.pass_count,
                    fail_count: slot.fail_count,
                    watermark: slot.watermark,
                    frontier: slot.frontier,
                })
            })
            .collect()
    }
}

/// A snapshot of an assertion slot for reporting.
#[derive(Debug, Clone)]
pub struct AssertionSlotSnapshot {
    /// The assertion message.
    pub msg: String,
    /// The kind of assertion (`AssertKind` as u8).
    pub kind: u8,
    /// Whether this assertion must be hit.
    pub must_hit: u8,
    /// Number of times the assertion passed.
    pub pass_count: u64,
    /// Number of times the assertion failed.
    pub fail_count: u64,
    /// Best watermark value (for numeric assertions).
    pub watermark: i64,
    /// Frontier value (for `BooleanSometimesAll`).
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
        // msg_hash(4) + kind(1) + must_hit(1) + maximize(1) + split_triggered(1) +
        // pass_count(8) + fail_count(8) + watermark(8) + split_watermark(8) +
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
    fn test_assert_kind_from_u8() {
        assert_eq!(AssertKind::from_u8(0), Some(AssertKind::Always));
        assert_eq!(
            AssertKind::from_u8(7),
            Some(AssertKind::BooleanSometimesAll)
        );
        assert_eq!(AssertKind::from_u8(8), None);
    }
}
