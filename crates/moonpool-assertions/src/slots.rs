//! Rich assertion slot tracking for the Antithesis-style assertion suite.
//!
//! Maintains a fixed-size table of assertion slots. Supports boolean assertions
//! (always/sometimes/reachable/unreachable), numeric guidance assertions (with
//! watermark tracking), and compound boolean assertions (sometimes-all with
//! frontier tracking).
//!
//! Each slot is accessed via raw pointer arithmetic on the assertion region
//! (heap by default, or `MAP_SHARED` memory when an exploration backend installs
//! one). Slot metadata is fully initialized before a release-store publishes
//! it, so concurrent readers never observe a partially initialized slot.
//!
//! On a "discovery" (first Sometimes/Reachable pass, numeric watermark
//! improvement, or frontier advance) the accounting calls
//! [`crate::hooks::on_discovery`]. Each discovery is guarded by an atomic
//! latch so it fires exactly once globally. With no hook installed this is a
//! no-op (pure accounting); the exploration backend wires it to a per-run
//! discovery journal.

use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};

/// Maximum number of tracked assertion slots.
pub const MAX_ASSERTION_SLOTS: usize = 128;

/// Maximum length of the assertion message stored in a slot.
const SLOT_MSG_LEN: usize = 64;

const SLOT_INITIALIZING: u8 = 1;
const SLOT_READY: u8 = 2;

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
    /// Whether this assertion has made its first discovery (0 = no, 1 = yes).
    pub discovered: u8,
    /// Total number of times this assertion passed.
    pub pass_count: u64,
    /// Total number of times this assertion failed.
    pub fail_count: u64,
    /// Numeric watermark: best value observed (for guidance assertions).
    pub watermark: i64,
    /// Watermark value at the last signalled discovery (for improvement detection).
    pub discovery_watermark: i64,
    /// Frontier: number of simultaneously true bools (for `BooleanSometimesAll`).
    pub frontier: u8,
    /// Publication state: zero = unused, one = initializing, two = ready.
    published: u8,
    /// Padding for alignment.
    pad: [u8; 6],
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

/// Update a monotonic watermark, returning whether this call advanced it.
fn update_watermark(watermark: &AtomicI64, value: i64, maximize: bool) -> bool {
    let previous = if maximize {
        watermark.fetch_max(value, Ordering::Relaxed)
    } else {
        watermark.fetch_min(value, Ordering::Relaxed)
    };
    if maximize {
        value > previous
    } else {
        value < previous
    }
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

        // Search only fully published slots. The acquire pairs with the
        // release-store below, making immutable metadata safe to read.
        for i in 0..count.min(MAX_ASSERTION_SLOTS) {
            let slot = base.add(i);
            let published = &*std::ptr::addr_of!((*slot).published).cast::<AtomicU8>();
            let state = published.load(Ordering::Acquire);
            let h = &*std::ptr::addr_of!((*slot).msg_hash).cast::<AtomicU32>();
            if state == SLOT_READY && h.load(Ordering::Relaxed) == hash {
                return (slot, i);
            }
            if state == SLOT_INITIALIZING && h.load(Ordering::Relaxed) == hash {
                return (std::ptr::null_mut(), 0);
            }
        }

        // Allocate new slot atomically.
        let new_idx = next_atomic.fetch_add(1, Ordering::AcqRel) as usize;
        if new_idx >= MAX_ASSERTION_SLOTS {
            next_atomic.fetch_sub(1, Ordering::AcqRel);
            return (std::ptr::null_mut(), 0);
        }

        let slot = base.add(new_idx);
        let slot_hash = &*std::ptr::addr_of!((*slot).msg_hash).cast::<AtomicU32>();
        let published = &*std::ptr::addr_of!((*slot).published).cast::<AtomicU8>();
        slot_hash.store(hash, Ordering::Relaxed);
        published.store(SLOT_INITIALIZING, Ordering::Release);

        // A published claim wins immediately; among simultaneous initializers,
        // the lower index wins deterministically.
        let claimed = next_atomic.load(Ordering::Acquire) as usize;
        for i in 0..claimed.min(MAX_ASSERTION_SLOTS) {
            if i == new_idx {
                continue;
            }
            let existing = base.add(i);
            let existing_state = &*std::ptr::addr_of!((*existing).published).cast::<AtomicU8>();
            let state = existing_state.load(Ordering::Acquire);
            if state == 0 {
                continue;
            }
            let existing_hash = &*std::ptr::addr_of!((*existing).msg_hash).cast::<AtomicU32>();
            if existing_hash.load(Ordering::Relaxed) != hash {
                continue;
            }
            if state == SLOT_INITIALIZING && i > new_idx {
                continue;
            }
            slot_hash.store(0, Ordering::Relaxed);
            published.store(0, Ordering::Release);
            return if state == SLOT_READY {
                (existing, i)
            } else {
                (std::ptr::null_mut(), 0)
            };
        }

        let mut msg_buf = [0u8; SLOT_MSG_LEN];
        let n = msg.len().min(SLOT_MSG_LEN - 1);
        msg_buf[..n].copy_from_slice(&msg.as_bytes()[..n]);

        (*slot).kind = kind as u8;
        (*slot).must_hit = must_hit;
        (*slot).maximize = maximize;
        (*slot).discovered = 0;
        (*slot).pass_count = 0;
        (*slot).fail_count = 0;
        (*slot).watermark = if maximize == 1 { i64::MIN } else { i64::MAX };
        (*slot).discovery_watermark = if maximize == 1 { i64::MIN } else { i64::MAX };
        (*slot).frontier = 0;
        (*slot).pad = [0; 6];
        (*slot).msg = msg_buf;

        published.store(SLOT_READY, Ordering::Release);

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
    let (slot, _slot_idx) =
        unsafe { find_or_alloc_slot(table_ptr, hash, kind, must_hit_u8, 0, msg) };
    if slot.is_null() {
        return;
    }

    // Safety: slot points to valid memory.
    unsafe {
        match kind {
            AssertKind::Always | AssertKind::AlwaysOrUnreachable | AssertKind::NumericAlways => {
                if condition {
                    let pc = &*(&raw const (*slot).pass_count).cast::<AtomicU64>();
                    pc.fetch_add(1, Ordering::Relaxed);
                } else {
                    let fc = &*(&raw const (*slot).fail_count).cast::<AtomicU64>();
                    let prev = fc.fetch_add(1, Ordering::Relaxed);
                    if prev == 0 {
                        eprintln!("[ASSERTION FAILED] {msg} (kind={kind:?})");
                    }
                }
            }
            AssertKind::Sometimes | AssertKind::Reachable => {
                if condition {
                    let pc = &*(&raw const (*slot).pass_count).cast::<AtomicU64>();
                    pc.fetch_add(1, Ordering::Relaxed);

                    // CAS discovered from 0 → 1 on first success
                    let ft = &*(&raw const (*slot).discovered).cast::<AtomicU8>();
                    if ft
                        .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        crate::hooks::on_discovery(
                            crate::hooks::DiscoveryKind::SometimesPass,
                            u64::from(hash),
                        );
                    }
                } else {
                    let fc = &*(&raw const (*slot).fail_count).cast::<AtomicU64>();
                    fc.fetch_add(1, Ordering::Relaxed);
                }
            }
            AssertKind::Unreachable => {
                // Being reached at all is a "pass" (the assertion is that we should NOT reach)
                // We track it as pass_count = times reached (bad), fail_count unused
                let pc = &*(&raw const (*slot).pass_count).cast::<AtomicU64>();
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
/// the last discovery watermark.
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
    let (slot, _slot_idx) =
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
            let pc = &*(&raw const (*slot).pass_count).cast::<AtomicU64>();
            pc.fetch_add(1, Ordering::Relaxed);
        } else {
            let fc = &*(&raw const (*slot).fail_count).cast::<AtomicU64>();
            let prev = fc.fetch_add(1, Ordering::Relaxed);
            if kind == AssertKind::NumericAlways && prev == 0 {
                eprintln!(
                    "[NUMERIC ASSERTION FAILED] {msg} (left={left}, right={right}, cmp={cmp:?})"
                );
            }
        }

        // Update watermark: track best value of `left`
        let wm = &*(&raw const (*slot).watermark).cast::<AtomicI64>();
        update_watermark(wm, left, maximize);

        // For NumericSometimes: signal discovery when watermark improves past discovery_watermark
        if kind == AssertKind::NumericSometimes {
            let fw = &*(&raw const (*slot).discovery_watermark).cast::<AtomicI64>();
            if update_watermark(fw, left, maximize) {
                crate::hooks::on_discovery(
                    crate::hooks::DiscoveryKind::WatermarkImprovement,
                    u64::from(hash),
                );
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
    let (slot, _slot_idx) =
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
        let pc = &*(&raw const (*slot).pass_count).cast::<AtomicU64>();
        pc.fetch_add(1, Ordering::Relaxed);

        // Signal discovery only for the caller that advances the shared frontier.
        let fr = &*(&raw const (*slot).frontier).cast::<AtomicU8>();
        if true_count > fr.fetch_max(true_count, Ordering::Relaxed) {
            crate::hooks::on_discovery(
                crate::hooks::DiscoveryKind::FrontierAdvance,
                u64::from(hash),
            );
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
    // - Mutable accounting fields are loaded atomically while immutable metadata
    //   is read only after acquiring the publication latch.
    unsafe {
        let count = (&*table_ptr.cast::<()>().cast::<AtomicU32>()).load(Ordering::Acquire) as usize;
        let count = count.min(MAX_ASSERTION_SLOTS);
        let base = table_ptr.add(8).cast::<()>().cast::<AssertionSlot>();

        (0..count)
            .filter_map(|i| {
                let slot = base.add(i);
                let published = &*std::ptr::addr_of!((*slot).published).cast::<AtomicU8>();
                if published.load(Ordering::Acquire) != SLOT_READY {
                    return None;
                }
                let message = std::ptr::read(std::ptr::addr_of!((*slot).msg));
                let message_len = message
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(SLOT_MSG_LEN);
                Some(AssertionSlotSnapshot {
                    msg: std::str::from_utf8(&message[..message_len])
                        .unwrap_or("???")
                        .to_string(),
                    kind: std::ptr::read(std::ptr::addr_of!((*slot).kind)),
                    must_hit: std::ptr::read(std::ptr::addr_of!((*slot).must_hit)),
                    pass_count: (&*std::ptr::addr_of!((*slot).pass_count).cast::<AtomicU64>())
                        .load(Ordering::Relaxed),
                    fail_count: (&*std::ptr::addr_of!((*slot).fail_count).cast::<AtomicU64>())
                        .load(Ordering::Relaxed),
                    watermark: (&*std::ptr::addr_of!((*slot).watermark).cast::<AtomicI64>())
                        .load(Ordering::Relaxed),
                    frontier: (&*std::ptr::addr_of!((*slot).frontier).cast::<AtomicU8>())
                        .load(Ordering::Relaxed),
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
        // msg_hash(4) + kind(1) + must_hit(1) + maximize(1) + discovered(1) +
        // pass_count(8) + fail_count(8) + watermark(8) + discovery_watermark(8) +
        // frontier(1) + published(1) + _pad(6) + msg(64) = 112
        assert_eq!(std::mem::size_of::<AssertionSlot>(), 112);
    }

    #[test]
    fn snapshots_skip_slots_until_metadata_is_published() {
        crate::region::init();
        crate::region::reset();
        let table = crate::region::assertion_table_ptr();

        // Simulate an allocator that reserved index zero but has not finished
        // initializing its metadata. Readers must not expose the zeroed slot.
        // Safety: `init` installed a correctly aligned table region.
        unsafe {
            (&*table.cast::<()>().cast::<AtomicU32>()).store(1, Ordering::Release);
        }
        assert!(assertion_read_all().is_empty());
        crate::region::clear();
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

    #[test]
    fn test_numeric_watermarks_keep_best_value() {
        crate::region::init();

        for value in [10, 5, 20] {
            assertion_numeric(
                AssertKind::NumericSometimes,
                AssertCmp::Gt,
                true,
                value,
                0,
                "maximize",
            );
        }
        for value in [10, 20, 5] {
            assertion_numeric(
                AssertKind::NumericSometimes,
                AssertCmp::Gt,
                false,
                value,
                0,
                "minimize",
            );
        }

        let slots = assertion_read_all();
        let maximize = slots
            .iter()
            .find(|slot| slot.msg == "maximize")
            .expect("maximize slot recorded");
        let minimize = slots
            .iter()
            .find(|slot| slot.msg == "minimize")
            .expect("minimize slot recorded");
        assert_eq!(maximize.watermark, 20);
        assert_eq!(minimize.watermark, 5);

        crate::region::clear();
    }

    #[test]
    fn test_sometimes_all_frontier_only_moves_forward() {
        crate::region::init();

        assertion_sometimes_all("frontier", &[("a", true), ("b", false), ("c", false)]);
        assertion_sometimes_all("frontier", &[("a", false), ("b", false), ("c", false)]);
        assertion_sometimes_all("frontier", &[("a", true), ("b", true), ("c", false)]);

        let slots = assertion_read_all();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].frontier, 2);
        assert_eq!(slots[0].pass_count, 3);

        crate::region::clear();
    }
}
