//! The publication protocol shared by the slot table and the each-bucket table.
//!
//! Both regions are laid out as `[count: u32, header: u32, entries: [E; N]]`.
//! `count` is incremented atomically (via `AtomicU32::fetch_add`) to claim an
//! entry safely across process boundaries; every entry carries an identity
//! and a publication byte (zero = unused, one = initializing, two = ready).
//! Entry metadata is fully initialized before a release-store publishes it, so
//! concurrent readers never observe a partially initialized entry.

use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};

const UNUSED: u8 = 0;
const INITIALIZING: u8 = 1;
pub(crate) const READY: u8 = 2;

/// Byte offset of the entry array: the two `u32` header words precede it.
const ENTRIES_OFFSET: usize = 8;

/// A plain integer field of a region entry that is accessed atomically.
pub(crate) trait AtomicField {
    /// The atomic type with the same size and bit validity as `Self`.
    type Atomic;
}

impl AtomicField for u8 {
    type Atomic = AtomicU8;
}

impl AtomicField for u32 {
    type Atomic = AtomicU32;
}

impl AtomicField for u64 {
    type Atomic = AtomicU64;
}

impl AtomicField for i64 {
    type Atomic = AtomicI64;
}

/// View a field of a region entry as its atomic counterpart.
///
/// # Safety
///
/// `field` must point into a live, suitably aligned region that outlives `'a`.
pub(crate) unsafe fn atomic<'a, T: AtomicField>(field: *const T) -> &'a T::Atomic {
    unsafe { &*field.cast::<T::Atomic>() }
}

/// View one of the two `u32` header words (`index` 0 or 1) of a region.
///
/// # Safety
///
/// `region` must point to a live, eight-byte aligned table region.
pub(crate) unsafe fn header_word<'a>(region: *mut u8, index: usize) -> &'a AtomicU32 {
    unsafe { atomic(region.add(index * 4).cast::<()>().cast::<u32>()) }
}

/// An entry type laid out in a region table.
pub(crate) trait Entry: Sized {
    /// The identity the table deduplicates entries on.
    type Id: Copy + Eq + Default;

    /// Number of entries the region holds.
    const CAPACITY: usize;

    /// Pointer to the entry's publication byte.
    ///
    /// # Safety
    ///
    /// `entry` must point to an entry inside a live region.
    unsafe fn published(entry: *mut Self) -> *const u8;

    /// Load the entry's identity (relaxed).
    ///
    /// # Safety
    ///
    /// `entry` must point to an entry inside a live region.
    unsafe fn load_id(entry: *mut Self) -> Self::Id;

    /// Store the entry's identity (relaxed); `Id::default()` releases it.
    ///
    /// # Safety
    ///
    /// `entry` must point to an entry inside a live region.
    unsafe fn store_id(entry: *mut Self, id: Self::Id);
}

/// Outcome of [`claim`].
pub(crate) enum Claim<E> {
    /// A published entry with the requested identity.
    Entry(*mut E),
    /// Another claimant is still initializing the identity.
    Busy,
    /// The table is full.
    Full,
}

/// The claimed-entry count, clamped to the table capacity, and the entry base.
unsafe fn entries<E: Entry>(region: *mut u8) -> (usize, *mut E) {
    unsafe {
        let count = header_word(region, 0).load(Ordering::Acquire) as usize;
        let base = region.add(ENTRIES_OFFSET).cast::<()>().cast::<E>();
        (count.min(E::CAPACITY), base)
    }
}

/// Find the published entry for `id`, or claim a fresh one, fill it with
/// `init` and publish it.
///
/// # Safety
///
/// `region` must point to a live table region of `E` entries (at least
/// `ENTRIES_OFFSET + E::CAPACITY * size_of::<E>()` bytes).
pub(crate) unsafe fn claim<E: Entry>(
    region: *mut u8,
    id: E::Id,
    init: impl FnOnce(*mut E),
) -> Claim<E> {
    unsafe {
        let next = header_word(region, 0);
        let (count, base) = entries::<E>(region);

        // Search only claimed entries. The acquire pairs with the
        // release-stores below, making immutable metadata safe to read.
        for i in 0..count {
            let entry = base.add(i);
            let state = atomic(E::published(entry)).load(Ordering::Acquire);
            if state != UNUSED && E::load_id(entry) == id {
                return if state == READY {
                    Claim::Entry(entry)
                } else {
                    Claim::Busy
                };
            }
        }

        // Claim a new entry atomically.
        let new_idx = next.fetch_add(1, Ordering::AcqRel) as usize;
        if new_idx >= E::CAPACITY {
            next.fetch_sub(1, Ordering::AcqRel);
            return Claim::Full;
        }

        let entry = base.add(new_idx);
        let published = atomic(E::published(entry));
        E::store_id(entry, id);
        published.store(INITIALIZING, Ordering::Release);

        // A published claim wins immediately; among simultaneous initializers,
        // the lower index wins deterministically.
        let (claimed, _) = entries::<E>(region);
        for i in (0..claimed).filter(|&i| i != new_idx) {
            let existing = base.add(i);
            let state = atomic(E::published(existing)).load(Ordering::Acquire);
            if state == UNUSED
                || E::load_id(existing) != id
                || (state == INITIALIZING && i > new_idx)
            {
                continue;
            }
            E::store_id(entry, E::Id::default());
            published.store(UNUSED, Ordering::Release);
            return if state == READY {
                Claim::Entry(existing)
            } else {
                Claim::Busy
            };
        }

        init(entry);
        published.store(READY, Ordering::Release);
        Claim::Entry(entry)
    }
}

/// Every published entry of the region, in claim order.
///
/// # Safety
///
/// Same contract as [`claim`]; the region must outlive the iterator.
pub(crate) unsafe fn published_entries<E: Entry>(region: *mut u8) -> impl Iterator<Item = *mut E> {
    // Safety: the loop bound keeps `base.add(i)` inside the region, and the
    // acquire on the publication byte makes the entry's metadata readable.
    unsafe {
        let (count, base) = entries::<E>(region);
        (0..count)
            .map(move |i| base.add(i))
            .filter(|&entry| atomic(E::published(entry)).load(Ordering::Acquire) == READY)
    }
}

/// Copy `msg` into a NUL-terminated buffer, truncating it to `N - 1` bytes.
pub(crate) fn msg_buf<const N: usize>(msg: &str) -> [u8; N] {
    let mut buf = [0u8; N];
    let n = msg.len().min(N - 1);
    buf[..n].copy_from_slice(&msg.as_bytes()[..n]);
    buf
}

/// Read a NUL-terminated message buffer (`"???"` if it is not UTF-8).
pub(crate) fn msg_str(buf: &[u8]) -> &str {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..len]).unwrap_or("???")
}
