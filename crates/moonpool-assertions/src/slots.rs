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
//! it, so concurrent readers never observe a partially initialized slot (see
//! the private `table` module).
//!
//! On a "discovery" (first Sometimes/Reachable pass, numeric watermark
//! improvement, frontier advance, or new partial boolean combination) the accounting calls
//! `on_discovery` (in the `hooks` module). Each discovery is guarded by an atomic
//! latch so it fires exactly once globally. With no hook installed this is a
//! no-op (pure accounting); the exploration backend wires it to a per-run
//! discovery journal.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::hooks::{DiscoveryKind, on_discovery};
use crate::table::{self, Claim, Entry, atomic};

/// Maximum number of tracked assertion slots.
pub const MAX_ASSERTION_SLOTS: usize = 2048;

/// Maximum length of the assertion message stored in a slot.
const SLOT_MSG_LEN: usize = 64;

/// Total size of the assertion table memory region in bytes.
///
/// Layout: `[next_slot: u32, dropped_allocations: u32, slots: [AssertionSlot; MAX_ASSERTION_SLOTS]]`
pub const ASSERTION_TABLE_MEM_SIZE: usize =
    8 + MAX_ASSERTION_SLOTS * std::mem::size_of::<AssertionSlot>();

/// Index of the dropped-allocation counter among the table's header words.
const DROPPED_ALLOCATIONS_WORD: usize = 1;

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

impl AssertCmp {
    fn holds(self, left: i64, right: i64) -> bool {
        match self {
            Self::Gt => left > right,
            Self::Ge => left >= right,
            Self::Lt => left < right,
            Self::Le => left <= right,
        }
    }
}

/// A single assertion tracking slot.
///
/// All fields are accessed via raw pointer arithmetic on the assertion region.
#[repr(C)]
pub struct AssertionSlot {
    /// 64-bit FNV-1a hash of the assertion message — the slot's identity.
    pub msg_hash: u64,
    /// Total number of times this assertion passed.
    pub pass_count: u64,
    /// Total number of times this assertion failed.
    pub fail_count: u64,
    /// Numeric watermark: best value observed (for guidance assertions).
    pub watermark: i64,
    /// Watermark value at the last signalled discovery (for improvement detection).
    pub discovery_watermark: i64,
    /// Bounded bloom bitmap of partial combinations seen by `sometimes_all`.
    pub combination_bits: u64,
    /// The kind of assertion (`AssertKind` as u8).
    pub kind: u8,
    /// Whether this assertion must be hit (1) or not (0).
    pub must_hit: u8,
    /// Whether to maximize (1) or minimize (0) the watermark value.
    pub maximize: u8,
    /// Whether this assertion has made its first discovery (0 = no, 1 = yes).
    pub discovered: u8,
    /// Frontier: number of simultaneously true bools (for `BooleanSometimesAll`).
    pub frontier: u8,
    /// Number of propositions in a `BooleanSometimesAll` assertion.
    pub frontier_target: u8,
    /// Publication state: zero = unused, one = initializing, two = ready.
    published: u8,
    /// Padding for alignment.
    pad: [u8; 1],
    /// Assertion message string (null-terminated).
    pub msg: [u8; SLOT_MSG_LEN],
}

impl AssertionSlot {
    /// Get the assertion message as a string slice.
    #[must_use]
    pub fn msg_str(&self) -> &str {
        table::msg_str(&self.msg)
    }
}

impl Entry for AssertionSlot {
    type Id = u64;
    const CAPACITY: usize = MAX_ASSERTION_SLOTS;

    unsafe fn published(entry: *mut Self) -> *const u8 {
        unsafe { &raw const (*entry).published }
    }

    unsafe fn load_id(entry: *mut Self) -> u64 {
        unsafe { atomic(&raw const (*entry).msg_hash).load(Ordering::Relaxed) }
    }

    unsafe fn store_id(entry: *mut Self, id: u64) {
        unsafe { atomic(&raw const (*entry).msg_hash).store(id, Ordering::Relaxed) }
    }
}

/// 64-bit FNV-1a offset basis.
const FNV64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a (64-bit) continuation of `hash` over `bytes`.
pub(crate) fn fnv1a_64(hash: u64, bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(hash, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// 64-bit FNV-1a hash of a message string — the stable identity of an
/// assertion site.
///
/// Two messages that hash alike share one slot and silently merge their
/// accounting, so the hash is 64 bits wide: across a full table of
/// [`MAX_ASSERTION_SLOTS`] sites the birthday bound puts the chance of any
/// collision near 10⁻¹³, where 32 bits put it near 10⁻³.
#[must_use]
pub fn msg_hash(msg: &str) -> u64 {
    fnv1a_64(FNV64_OFFSET, msg.bytes())
}

/// Stable fingerprint for one complete set of named boolean observations.
fn boolean_combination_fingerprint(named_bools: &[(&str, bool)]) -> u64 {
    // FNV-1a (64-bit) over each name, a 0xff separator, and the value byte.
    fnv1a_64(
        FNV64_OFFSET,
        named_bools
            .iter()
            .flat_map(|&(name, value)| name.bytes().chain([0xff, u8::from(value)])),
    )
}

/// Mix a site and proposition fingerprint into one semantic state id.
fn boolean_combination_state_id(site_hash: u64, fingerprint: u64) -> u64 {
    fingerprint ^ site_hash.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// Update a monotonic watermark, returning whether this call advanced it.
fn update_watermark(watermark: &AtomicI64, value: i64, maximize: bool) -> bool {
    if maximize {
        value > watermark.fetch_max(value, Ordering::Relaxed)
    } else {
        value < watermark.fetch_min(value, Ordering::Relaxed)
    }
}

/// Find an existing slot for `msg` or allocate a new one.
///
/// Returns the slot and its message hash, or `None` when the assertion table
/// is not initialized, the slot is still being initialized by another
/// claimant, or the table is full (counted as a dropped allocation).
fn find_or_alloc_slot(
    kind: AssertKind,
    must_hit: bool,
    maximize: bool,
    msg: &str,
) -> Option<(*mut AssertionSlot, u64)> {
    let table_ptr = crate::region::assertion_table_ptr();
    if table_ptr.is_null() {
        return None;
    }
    let hash = msg_hash(msg);
    let init = |slot: *mut AssertionSlot| {
        let start = if maximize { i64::MIN } else { i64::MAX };
        // Safety: `claim` hands out an unpublished slot this call owns.
        unsafe {
            (*slot).kind = kind as u8;
            (*slot).must_hit = u8::from(must_hit);
            (*slot).maximize = u8::from(maximize);
            (*slot).discovered = 0;
            (*slot).pass_count = 0;
            (*slot).fail_count = 0;
            (*slot).watermark = start;
            (*slot).discovery_watermark = start;
            (*slot).combination_bits = 0;
            (*slot).frontier = 0;
            (*slot).frontier_target = 0;
            (*slot).pad = [0; 1];
            (*slot).msg = table::msg_buf(msg);
        }
    };
    // Safety: table_ptr points to ASSERTION_TABLE_MEM_SIZE bytes.
    match unsafe { table::claim(table_ptr, hash, init) } {
        Claim::Entry(slot) => Some((slot, hash)),
        Claim::Busy => None,
        Claim::Full => {
            // Safety: the second header word is the dropped-allocation counter.
            let dropped = unsafe { table::header_word(table_ptr, DROPPED_ALLOCATIONS_WORD) };
            let _ = dropped.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
            None
        }
    }
}

/// Bump the slot's pass (`passed`) or fail counter, returning its previous value.
///
/// # Safety
///
/// `slot` must point to a published slot of a live assertion table.
unsafe fn record(slot: *mut AssertionSlot, passed: bool) -> u64 {
    unsafe {
        let counter = if passed {
            &raw const (*slot).pass_count
        } else {
            &raw const (*slot).fail_count
        };
        atomic(counter).fetch_add(1, Ordering::Relaxed)
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
    let Some((slot, hash)) = find_or_alloc_slot(kind, must_hit, false, msg) else {
        return;
    };

    // Safety: slot points to valid memory.
    unsafe {
        match kind {
            AssertKind::Always | AssertKind::AlwaysOrUnreachable | AssertKind::NumericAlways => {
                let previous = record(slot, condition);
                if !condition && previous == 0 {
                    eprintln!("[ASSERTION FAILED] {msg} (kind={kind:?})");
                }
            }
            AssertKind::Sometimes | AssertKind::Reachable => {
                record(slot, condition);
                // CAS discovered from 0 → 1 on first success
                if condition
                    && atomic(&raw const (*slot).discovered)
                        .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    on_discovery(DiscoveryKind::SometimesPass, hash);
                }
            }
            AssertKind::Unreachable => {
                // Being reached at all is a "pass" (the assertion is that we should NOT reach)
                // We track it as pass_count = times reached (bad), fail_count unused
                let previous = record(slot, true);
                if previous == 0 {
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
    let Some((slot, hash)) = find_or_alloc_slot(kind, true, maximize, msg) else {
        return;
    };
    let passes = cmp.holds(left, right);

    // Safety: slot points to valid memory.
    unsafe {
        let previous = record(slot, passes);
        if !passes && kind == AssertKind::NumericAlways && previous == 0 {
            eprintln!("[NUMERIC ASSERTION FAILED] {msg} (left={left}, right={right}, cmp={cmp:?})");
        }

        // Update watermark: track best value of `left`
        update_watermark(atomic(&raw const (*slot).watermark), left, maximize);

        // Numeric guidance follows the comparison distance (`left - right`),
        // matching Antithesis: a changing threshold must influence whether an
        // observation is actually closer to satisfying the property. The
        // public watermark above deliberately remains the best `left` value
        // for human-readable reporting.
        if kind == AssertKind::NumericSometimes
            && update_watermark(
                atomic(&raw const (*slot).discovery_watermark),
                left.saturating_sub(right),
                maximize,
            )
        {
            on_discovery(DiscoveryKind::WatermarkImprovement, hash);
        }
    }
}

/// Compound boolean assertion backing function (sometimes-all).
///
/// Counts how many of the named booleans are simultaneously true. Maintains a
/// frontier (max count seen) and a bounded bitmap of distinct partial truth
/// combinations. Signals a discovery when the frontier advances or a new
/// same-frontier combination is observed.
///
/// This is a no-op if the assertion table is not initialized.
pub fn assertion_sometimes_all(msg: &str, named_bools: &[(&str, bool)]) {
    let Some((slot, hash)) = find_or_alloc_slot(AssertKind::BooleanSometimesAll, true, false, msg)
    else {
        return;
    };

    // Count simultaneously true bools. The frontier field is u8, so we cap at u8::MAX —
    // callers passing more than 255 named bools is not a supported use case; clamp
    // via `unwrap_or(u8::MAX)` so we never panic.
    let true_count =
        u8::try_from(named_bools.iter().filter(|(_, v)| *v).count()).unwrap_or(u8::MAX);
    let target = u8::try_from(named_bools.len()).unwrap_or(u8::MAX);
    let fingerprint = boolean_combination_fingerprint(named_bools);
    let combination_bit = 1_u64 << (fingerprint & 63);

    // Safety: slot points to valid memory.
    unsafe {
        // Increment pass_count (always, for statistics)
        record(slot, true);

        atomic(&raw const (*slot).frontier_target).fetch_max(target, Ordering::Relaxed);

        // Record the partial combination even when a frontier advance wins the
        // discovery for this encounter. That prevents the same combination
        // from producing a redundant event on its next encounter. A 64-bit
        // bloom bitmap bounds guidance per site; collisions lose optional
        // combination hints but never assertion accounting or frontier data.
        let previous_combinations = atomic(&raw const (*slot).combination_bits)
            .fetch_or(combination_bit, Ordering::Relaxed);
        let new_combination = previous_combinations & combination_bit == 0;

        let frontier = atomic(&raw const (*slot).frontier);
        if true_count > frontier.fetch_max(true_count, Ordering::Relaxed) {
            on_discovery(DiscoveryKind::FrontierAdvance, hash);
        } else if true_count > 0 && new_combination {
            on_discovery(
                DiscoveryKind::BooleanCombination,
                boolean_combination_state_id(hash, fingerprint),
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
    // Mutable accounting fields are loaded atomically while immutable metadata
    // is read only after acquiring the publication latch.
    unsafe {
        table::published_entries::<AssertionSlot>(table_ptr)
            .map(|slot| AssertionSlotSnapshot {
                msg: table::msg_str(&(*slot).msg).to_string(),
                kind: (*slot).kind,
                must_hit: (*slot).must_hit,
                pass_count: atomic(&raw const (*slot).pass_count).load(Ordering::Relaxed),
                fail_count: atomic(&raw const (*slot).fail_count).load(Ordering::Relaxed),
                watermark: atomic(&raw const (*slot).watermark).load(Ordering::Relaxed),
                combinations_seen: atomic(&raw const (*slot).combination_bits)
                    .load(Ordering::Relaxed)
                    .count_ones(),
                frontier: atomic(&raw const (*slot).frontier).load(Ordering::Relaxed),
                frontier_target: atomic(&raw const (*slot).frontier_target).load(Ordering::Relaxed),
            })
            .collect()
    }
}

/// Read the number of assertion evaluations that could not allocate a slot.
///
/// The counter lives in the assertion table header, so exploration timelines
/// backed by `MAP_SHARED` memory contribute to one cumulative value. Returns
/// zero if the assertion table is not initialized.
#[must_use]
pub fn assertion_dropped_allocations() -> u32 {
    let table_ptr = crate::region::assertion_table_ptr();
    if table_ptr.is_null() {
        return 0;
    }

    // Safety: table_ptr points to ASSERTION_TABLE_MEM_SIZE bytes, and the
    // second u32 in the table header is the dropped-allocation counter.
    unsafe { table::header_word(table_ptr, DROPPED_ALLOCATIONS_WORD).load(Ordering::Relaxed) }
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
    /// Number of distinct partial-combination bloom bits observed.
    pub combinations_seen: u32,
    /// Frontier value (for `BooleanSometimesAll`).
    pub frontier: u8,
    /// Frontier target (the number of `BooleanSometimesAll` propositions).
    pub frontier_target: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static TEST_DISCOVERIES: RefCell<Vec<(crate::hooks::DiscoveryKind, u64)>> =
            const { RefCell::new(Vec::new()) };
    }

    fn record_discovery(kind: crate::hooks::DiscoveryKind, state_id: u64) {
        TEST_DISCOVERIES.with(|events| events.borrow_mut().push((kind, state_id)));
    }

    fn take_discoveries() -> Vec<(crate::hooks::DiscoveryKind, u64)> {
        TEST_DISCOVERIES.with(|events| std::mem::take(&mut *events.borrow_mut()))
    }

    #[test]
    fn test_msg_hash_deterministic() {
        let h1 = msg_hash("test_assertion");
        let h2 = msg_hash("test_assertion");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_msg_hash_no_collision() {
        let names = ["a", "b", "c", "timeout", "connect", "retry"];
        let hashes: Vec<u64> = names.iter().map(|n| msg_hash(n)).collect();
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
    fn hashes_are_a_stable_wire_format() {
        // Slot identities and discovery ids are persisted by campaigns
        // downstream; these values must never change.
        assert_eq!(msg_hash("site"), 0x4dd5_aa18_e606_742e);
        assert_eq!(
            boolean_combination_fingerprint(&[("a", true), ("b", false)]),
            0xd805_8bf7_0e22_3339
        );
    }

    #[test]
    fn test_slot_size_stable() {
        // Verify AssertionSlot size for shared memory layout stability.
        // msg_hash(8) + pass_count(8) + fail_count(8) + watermark(8) +
        // discovery_watermark(8) + combination_bits(8) + kind(1) + must_hit(1) +
        // maximize(1) + discovered(1) + frontier(1) + frontier_target(1) +
        // published(1) + _pad(1) + msg(64) = 120
        assert_eq!(std::mem::size_of::<AssertionSlot>(), 120);
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
            crate::table::header_word(table, 0).store(1, Ordering::Release);
        }
        assert!(assertion_read_all().is_empty());
        crate::region::clear();
    }

    #[test]
    fn tracks_more_than_the_legacy_slot_limit() {
        crate::region::init();
        crate::region::reset();

        for index in 0..129 {
            assertion_bool(
                AssertKind::Sometimes,
                true,
                true,
                &format!("legacy capacity assertion {index}"),
            );
        }

        assert_eq!(assertion_read_all().len(), 129);
        assert_eq!(assertion_dropped_allocations(), 0);
        crate::region::clear();
    }

    #[test]
    fn counts_allocations_dropped_after_the_table_is_full() {
        crate::region::init();
        crate::region::reset();

        for index in 0..MAX_ASSERTION_SLOTS {
            assertion_bool(
                AssertKind::Sometimes,
                true,
                true,
                &format!("capacity assertion {index}"),
            );
        }
        assert_eq!(assertion_read_all().len(), MAX_ASSERTION_SLOTS);

        assertion_bool(AssertKind::Sometimes, true, true, "first dropped assertion");
        assertion_bool(
            AssertKind::Sometimes,
            true,
            true,
            "second dropped assertion",
        );

        assert_eq!(assertion_dropped_allocations(), 2);
        crate::region::reset();
        assert_eq!(assertion_dropped_allocations(), 0);
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
    fn numeric_guidance_tracks_comparison_distance_when_threshold_moves() {
        crate::region::init();
        crate::hooks::set_discovery_hooks(crate::hooks::DiscoveryHooks {
            on_discovery: record_discovery,
        });
        let _ = take_discoveries();

        assertion_numeric(
            AssertKind::NumericSometimes,
            AssertCmp::Gt,
            true,
            10,
            5,
            "dynamic threshold",
        );
        // The left operand improves, but the comparison distance falls from
        // 5 to -10, so this must not guide the explorer in the wrong direction.
        assertion_numeric(
            AssertKind::NumericSometimes,
            AssertCmp::Gt,
            true,
            20,
            30,
            "dynamic threshold",
        );
        assertion_numeric(
            AssertKind::NumericSometimes,
            AssertCmp::Gt,
            true,
            21,
            10,
            "dynamic threshold",
        );

        let discoveries = take_discoveries();
        assert_eq!(discoveries.len(), 2);
        assert!(
            discoveries
                .iter()
                .all(|(kind, _)| { *kind == crate::hooks::DiscoveryKind::WatermarkImprovement })
        );
        let slots = assertion_read_all();
        assert_eq!(slots[0].watermark, 21, "reports still show best left value");

        crate::hooks::clear_discovery_hooks();
        crate::region::clear();
    }

    #[test]
    fn sometimes_all_tracks_frontier_target_and_distinct_partial_combinations() {
        crate::region::init();
        crate::hooks::set_discovery_hooks(crate::hooks::DiscoveryHooks {
            on_discovery: record_discovery,
        });
        let _ = take_discoveries();

        assertion_sometimes_all("frontier", &[("a", true), ("b", false), ("c", false)]);
        // Same score as the first encounter, different proposition. Count-only
        // guidance used to collapse these two semantically different states.
        assertion_sometimes_all("frontier", &[("a", false), ("b", true), ("c", false)]);
        assertion_sometimes_all("frontier", &[("a", true), ("b", false), ("c", false)]);
        assertion_sometimes_all("frontier", &[("a", true), ("b", true), ("c", false)]);

        let slots = assertion_read_all();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].frontier, 2);
        assert_eq!(slots[0].frontier_target, 3);
        assert_eq!(slots[0].combinations_seen, 3);
        assert_eq!(slots[0].pass_count, 4);

        let discoveries = take_discoveries();
        assert_eq!(
            discoveries
                .iter()
                .filter(|(kind, _)| *kind == crate::hooks::DiscoveryKind::FrontierAdvance)
                .count(),
            2
        );
        assert_eq!(
            discoveries
                .iter()
                .filter(|(kind, _)| *kind == crate::hooks::DiscoveryKind::BooleanCombination)
                .count(),
            1
        );

        crate::hooks::clear_discovery_hooks();
        crate::region::clear();
    }
}
