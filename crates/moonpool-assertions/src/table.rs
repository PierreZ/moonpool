//! The publication protocol shared by the slot table and the each-bucket table.
//!
//! Both regions are laid out as `[count: u32, dropped: u32, entries: [E; N]]`.
//! The entries are an open-addressed hash table: an identity lives at its
//! hash modulo `N` or at the next free entry after it. Every entry carries an
//! identity and a publication byte (zero = unused, one = initializing, two =
//! ready). A claimant takes an entry with a compare-and-swap of that byte, so
//! a claim is safe across process boundaries. Entry metadata is fully
//! initialized before a release-store publishes it, so concurrent readers
//! never observe a partially initialized entry. `count` is the number of
//! claimed entries, kept for diagnostics.

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

/// Index of the dropped-allocation counter among a region's header words.
const DROPPED_ALLOCATIONS_WORD: usize = 1;

/// Count one allocation a full or contended region could not track.
///
/// # Safety
///
/// `region` must point to a live, eight-byte aligned table region.
pub(crate) unsafe fn count_dropped(region: *mut u8) {
    // Safety: the caller guarantees a live region; the second header word is
    // the dropped-allocation counter.
    let dropped = unsafe { header_word(region, DROPPED_ALLOCATIONS_WORD) };
    let _ = dropped.try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
        Some(count.saturating_add(1))
    });
}

/// Read a region's dropped-allocation counter.
///
/// # Safety
///
/// `region` must point to a live, eight-byte aligned table region.
pub(crate) unsafe fn dropped_allocations(region: *mut u8) -> u32 {
    // Safety: the caller guarantees a live region.
    unsafe { header_word(region, DROPPED_ALLOCATIONS_WORD).load(Ordering::Relaxed) }
}

/// An entry type laid out in a region table.
pub(crate) trait Entry: Sized {
    /// The identity the table deduplicates entries on.
    type Id: Copy + Eq + Default;

    /// Number of entries the region holds.
    const CAPACITY: usize;

    /// The hash that places `id` in the table.
    fn home(id: Self::Id) -> u64;

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
    /// Another claimant claimed an entry for this identity (or for an
    /// identity it did not store yet) and did not publish it within the
    /// bounded wait: it most likely died mid-initialization.
    Busy,
    /// The table is full.
    Full,
}

/// How many times [`await_published`] re-reads the publication byte with a
/// plain spin before it starts to yield and sleep.
const BUSY_SPINS: u32 = 1 << 6;

/// How long [`await_published`] waits for another claimant to publish an
/// entry before it gives up on it.
///
/// An initializer holds an entry for a handful of plain stores. But the
/// scheduler can stop it between two stores, and the explorer shares the
/// table between forked processes, so a wait counted in spins is too short:
/// a context switch is longer than a few thousand spins. Only an initializer
/// that died between claiming and publishing (a forked worker killed
/// mid-store) exhausts this wait. The caller then counts the observation as
/// dropped rather than hanging.
#[cfg(not(target_family = "wasm"))]
const PUBLISH_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// Longest sleep between two reads in [`await_published`].
#[cfg(not(target_family = "wasm"))]
const MAX_PUBLISH_SLEEP: std::time::Duration = std::time::Duration::from_millis(1);

/// Wait, bounded by [`PUBLISH_WAIT`], for `published` to leave the
/// initializing state, and return the state it settled in.
///
/// The wait spins first, then yields, then sleeps with an exponential
/// backoff. On wasm there is one thread and no clock, so the wait only
/// spins: an entry can stay in the initializing state there only when its
/// initializer is gone.
fn await_published(published: &AtomicU8) -> u8 {
    for _ in 0..BUSY_SPINS {
        let state = published.load(Ordering::Acquire);
        if state != INITIALIZING {
            return state;
        }
        std::hint::spin_loop();
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let start = std::time::Instant::now();
        let mut sleep = std::time::Duration::from_micros(1);
        let mut yields = 0_u32;
        while start.elapsed() < PUBLISH_WAIT {
            let state = published.load(Ordering::Acquire);
            if state != INITIALIZING {
                return state;
            }
            if yields < BUSY_SPINS {
                yields += 1;
                std::thread::yield_now();
            } else {
                std::thread::sleep(sleep);
                sleep = (sleep * 2).min(MAX_PUBLISH_SLEEP);
            }
        }
    }
    published.load(Ordering::Acquire)
}

/// The entry base of a region.
unsafe fn base<E: Entry>(region: *mut u8) -> *mut E {
    unsafe { region.add(ENTRIES_OFFSET).cast::<()>().cast::<E>() }
}

/// The index where the probe sequence of `id` starts: its hash modulo the
/// capacity.
pub(crate) fn home_index<E: Entry>(id: E::Id) -> usize {
    // The modulo keeps the value below the capacity, which is a `usize`.
    usize::try_from(E::home(id) % E::CAPACITY as u64).unwrap_or(0)
}

/// Find the published entry for `id`, or claim a fresh one, fill it with
/// `init` and publish it.
///
/// The table is open-addressed: the search starts at
/// [`home_index`]`(id)` and probes the next entries in order, so a lookup
/// reads one or two entries, not every claimed one. A claimant takes an
/// unused entry with a compare-and-swap of its publication byte (unused to
/// initializing), stores the identity and the metadata, then publishes it
/// with a release-store. A reader that meets an entry still being
/// initialized waits for it, bounded by [`PUBLISH_WAIT`], before it reads
/// the identity. If the entry is still not published after the wait, and it
/// holds this identity or no identity yet, the result is [`Claim::Busy`]:
/// the probe never goes past an entry that can hold this identity, so one
/// identity never gets two entries. The table stays fixed-size and position-independent, so it
/// works in `MAP_SHARED` memory across forked processes.
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
        let base = base::<E>(region);
        let home = home_index::<E>(id);
        let mut init = Some(init);
        for probe in 0..E::CAPACITY {
            let entry = base.add((home + probe) % E::CAPACITY);
            let published = atomic(E::published(entry));
            let mut state = published.load(Ordering::Acquire);
            if state == UNUSED {
                match published.compare_exchange(
                    UNUSED,
                    INITIALIZING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        #[cfg(test)]
                        tests::pause_after_claim();
                        E::store_id(entry, id);
                        if let Some(init) = init.take() {
                            init(entry);
                        }
                        header_word(region, 0).fetch_add(1, Ordering::AcqRel);
                        published.store(READY, Ordering::Release);
                        return Claim::Entry(entry);
                    }
                    Err(current) => state = current,
                }
            }
            // The entry is taken. An entry still being initialized is waited
            // for, not skipped: it can hold this identity, and dropping the
            // observation would silently lose accounting.
            if state == INITIALIZING {
                state = await_published(published);
            }
            match state {
                READY if E::load_id(entry) == id => return Claim::Entry(entry),
                // The initializer did not publish within the bounded wait: it
                // most likely died mid-initialization. When the entry holds
                // this identity, or no identity yet (the initializer stopped
                // between its claim and its first store), the observation is
                // lost and counted. To go on would claim a second entry for
                // an identity that this entry can still hold, and one site
                // would then have two entries.
                INITIALIZING => {
                    let held = E::load_id(entry);
                    if held == id || held == E::Id::default() {
                        return Claim::Busy;
                    }
                }
                _ => {}
            }
        }
        Claim::Full
    }
}

/// Every published entry of the region, in table order.
///
/// # Safety
///
/// Same contract as [`claim`]; the region must outlive the iterator.
pub(crate) unsafe fn published_entries<E: Entry>(region: *mut u8) -> impl Iterator<Item = *mut E> {
    // Safety: the loop bound keeps `base.add(i)` inside the region, and the
    // acquire on the publication byte makes the entry's metadata readable.
    unsafe {
        let base = base::<E>(region);
        (0..E::CAPACITY)
            .map(move |i| base.add(i))
            .filter(|&entry| atomic(E::published(entry)).load(Ordering::Acquire) == READY)
    }
}

/// Copy `msg` into a NUL-terminated buffer, truncating it to at most `N - 1`
/// bytes on a character boundary so the stored prefix stays valid UTF-8.
pub(crate) fn msg_buf<const N: usize>(msg: &str) -> [u8; N] {
    let mut buf = [0u8; N];
    let n = msg.floor_char_boundary(N - 1);
    buf[..n].copy_from_slice(&msg.as_bytes()[..n]);
    buf
}

/// Read a NUL-terminated message buffer (`"???"` if it is not UTF-8).
pub(crate) fn msg_str(buf: &[u8]) -> &str {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..len]).unwrap_or("???")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::{
        Claim, Entry, INITIALIZING, UNUSED, atomic, base, claim, dropped_allocations, header_word,
        home_index, msg_buf, msg_str, published_entries,
    };
    use crate::slots::{ASSERTION_TABLE_MEM_SIZE, AssertionSlot};

    thread_local! {
        /// Delay of this thread between its claim of an entry and the store
        /// of the identity.
        static PAUSE: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    }

    /// Test hook in [`claim`]: stop the calling thread in the window between
    /// the claim of an entry and the store of its identity.
    pub(super) fn pause_after_claim() {
        let pause = PAUSE.get();
        if !pause.is_zero() {
            std::thread::sleep(pause);
        }
    }

    /// How long the slow initializer stops. It is much longer than a spin
    /// wait and shorter than [`super::PUBLISH_WAIT`].
    const SLOW_INIT: Duration = Duration::from_millis(30);

    const SITE: u64 = 0x5eed_0000_0000_0461;

    /// A zeroed slot-table region and its address.
    fn slot_region() -> (Vec<u64>, usize) {
        let mut memory = vec![0_u64; ASSERTION_TABLE_MEM_SIZE.div_ceil(8)];
        let address = memory.as_mut_ptr() as usize;
        (memory, address)
    }

    /// Claim `SITE` in the region at `address` and return the entry address,
    /// or `None` when the claim is busy or the table is full.
    fn claim_site(address: usize, init: impl FnOnce(*mut AssertionSlot)) -> Option<usize> {
        // Safety: `address` is a live, zeroed slot-table region.
        match unsafe { claim::<AssertionSlot>(address as *mut u8, SITE, init) } {
            Claim::Entry(entry) => Some(entry as usize),
            Claim::Busy | Claim::Full => None,
        }
    }

    /// Wait until the home entry of `SITE` is claimed and `ready` is true
    /// for it.
    fn wait_for_claim(address: usize, ready: impl Fn(*mut AssertionSlot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        // Safety: `address` is a live slot-table region; the index is in
        // bounds.
        let entry = unsafe {
            base::<AssertionSlot>(address as *mut u8).add(home_index::<AssertionSlot>(SITE))
        };
        loop {
            // Safety: `entry` points into the live region.
            let state = unsafe { atomic(AssertionSlot::published(entry)).load(Ordering::Acquire) };
            if state != UNUSED && ready(entry) {
                return;
            }
            assert!(Instant::now() < deadline, "the first claim never started");
            std::thread::yield_now();
        }
    }

    /// The region at `address` has exactly one published entry, it is the
    /// entry both claimants got, and nothing was dropped.
    fn assert_one_entry(address: usize, first: Option<usize>, second: Option<usize>) {
        assert!(first.is_some(), "the first claimant got no entry");
        assert_eq!(first, second, "the claimants did not get the same entry");
        let region = address as *mut u8;
        // Safety: `address` is a live slot-table region.
        unsafe {
            assert_eq!(published_entries::<AssertionSlot>(region).count(), 1);
            assert_eq!(header_word(region, 0).load(Ordering::Acquire), 1);
            assert_eq!(dropped_allocations(region), 0);
        }
    }

    /// A claimant stopped between its claim and the store of the identity
    /// must not make a second claimant take a second entry for the same
    /// site (#461 (slow slot initializer treated as dead)).
    #[test]
    fn a_claimant_stopped_before_its_id_store_keeps_one_entry() {
        let (_memory, address) = slot_region();
        let (first, second) = std::thread::scope(|scope| {
            let slow = scope.spawn(move || {
                PAUSE.set(SLOW_INIT);
                claim_site(address, |_| {})
            });
            wait_for_claim(address, |_| true);
            let second = claim_site(address, |_| {});
            (slow.join().expect("slow claimant"), second)
        });
        assert_one_entry(address, first, second);
    }

    /// A slow initializer that stored the identity must not make a second
    /// claimant drop its evaluation (#461).
    #[test]
    fn a_slow_initializer_with_its_id_stored_is_waited_for() {
        let (_memory, address) = slot_region();
        let (first, second) = std::thread::scope(|scope| {
            let slow = scope.spawn(move || claim_site(address, |_| std::thread::sleep(SLOW_INIT)));
            // Safety: the entry points into the live region.
            wait_for_claim(address, |entry| unsafe {
                AssertionSlot::load_id(entry) == SITE
                    && atomic(AssertionSlot::published(entry)).load(Ordering::Acquire)
                        == INITIALIZING
            });
            let second = claim_site(address, |_| {});
            (slow.join().expect("slow claimant"), second)
        });
        assert_one_entry(address, first, second);
    }

    #[test]
    fn msg_buf_truncates_on_a_char_boundary() {
        // "é" occupies bytes 2..4: a cut at byte 3 would split it.
        let buf = msg_buf::<4>("aaé");
        assert_eq!(msg_str(&buf), "aa");
    }

    #[test]
    fn msg_buf_keeps_short_messages_whole() {
        let buf = msg_buf::<8>("abc");
        assert_eq!(msg_str(&buf), "abc");
    }
}
