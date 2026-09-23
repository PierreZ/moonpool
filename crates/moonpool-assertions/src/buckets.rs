//! Per-value bucketed accounting for `assert_sometimes_each!`.
//!
//! Each unique combination of identity key values creates one bucket. On first
//! discovery — and on every quality-watermark improvement — the accounting
//! calls [`crate::hooks::on_discovery`]. With no hook installed the call is a
//! no-op (pure accounting); the exploration backend records it into a per-run
//! discovery journal.
//!
//! # Memory Layout
//!
//! ```text
//! [next_bucket: u32, _pad: u32, buckets: [EachBucket; MAX_EACH_BUCKETS]]
//! ```
//!
//! The `next_bucket` counter is incremented atomically (via `AtomicU32::fetch_add`)
//! to allocate new buckets safely across process boundaries.

use std::sync::atomic::Ordering;

use crate::hooks::{DiscoveryKind, on_discovery};
use crate::slots::{fnv1a_64, msg_hash};
use crate::table::{self, Claim, Entry, atomic};

/// Maximum number of `EachBucket` slots.
pub const MAX_EACH_BUCKETS: usize = 256;

/// Maximum number of identity keys per bucket.
pub const MAX_EACH_KEYS: usize = 6;

/// Maximum length of the assertion message stored in a bucket.
const EACH_MSG_LEN: usize = 32;

/// Total memory size for the `EachBucket` region.
pub const EACH_BUCKET_MEM_SIZE: usize = 8 + MAX_EACH_BUCKETS * std::mem::size_of::<EachBucket>();

/// One bucket's state for per-value bucketed assertions.
///
/// Each unique combination of identity key values creates one bucket.
/// Optional quality watermark (`has_quality != 0`): re-signals when `best_score` improves.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EachBucket {
    /// 64-bit FNV-1a hash of the assertion message string.
    pub site_hash: u64,
    /// Hash of (`site_hash` + identity key values) — uniquely identifies this bucket.
    pub bucket_hash: u64,
    /// CAS guard: 0 = not yet discovered, 1 = first discovery signalled.
    pub discovered: u8,
    /// Number of identity keys stored in `key_values`.
    pub num_keys: u8,
    /// Number of quality keys (0-4). 0 means no quality tracking.
    pub has_quality: u8,
    /// Publication state: zero = unused, one = initializing, two = ready.
    published: u8,
    /// Number of times this bucket has been hit (atomic increment).
    pub pass_count: u32,
    /// Best quality watermark score (atomic CAS for improvement detection).
    pub best_score: i64,
    /// Identity key values for display/debugging.
    pub key_values: [i64; MAX_EACH_KEYS],
    /// Assertion message string (null-terminated C-style).
    pub msg: [u8; EACH_MSG_LEN],
}

impl EachBucket {
    /// Get the assertion message as a string slice.
    #[must_use]
    pub fn msg_str(&self) -> &str {
        table::msg_str(&self.msg)
    }
}

impl Entry for EachBucket {
    /// (`site_hash`, `bucket_hash`).
    type Id = (u64, u64);
    const CAPACITY: usize = MAX_EACH_BUCKETS;

    unsafe fn published(entry: *mut Self) -> *const u8 {
        unsafe { &raw const (*entry).published }
    }

    unsafe fn load_id(entry: *mut Self) -> (u64, u64) {
        unsafe {
            (
                atomic(&raw const (*entry).site_hash).load(Ordering::Relaxed),
                atomic(&raw const (*entry).bucket_hash).load(Ordering::Relaxed),
            )
        }
    }

    unsafe fn store_id(entry: *mut Self, (site_hash, bucket_hash): (u64, u64)) {
        unsafe {
            atomic(&raw const (*entry).site_hash).store(site_hash, Ordering::Relaxed);
            atomic(&raw const (*entry).bucket_hash).store(bucket_hash, Ordering::Relaxed);
        }
    }
}

/// Bucket identity: `site_hash` mixed with the identity key values via FNV-1a.
/// Quality values are NOT included — they're watermarks, not identity keys.
fn bucket_hash(site_hash: u64, keys: &[(&str, i64)]) -> u64 {
    keys.iter()
        .fold(site_hash, |h, &(_, val)| fnv1a_64(h, val.to_le_bytes()))
}

/// Find an existing bucket or allocate a new one by (`site_hash`, `bucket_hash`).
///
/// Returns `None` if `EachBucket` memory is not initialized, the bucket is
/// still being initialized by another claimant, or the table is full.
fn find_or_alloc_each_bucket(
    site_hash: u64,
    bucket_hash: u64,
    keys: &[(&str, i64)],
    msg: &str,
    has_quality: u8,
) -> Option<*mut EachBucket> {
    let ptr = crate::region::each_bucket_ptr();
    if ptr.is_null() {
        return None;
    }
    let init = |bucket: *mut EachBucket| {
        let mut key_values = [0i64; MAX_EACH_KEYS];
        let num_keys = keys.len().min(MAX_EACH_KEYS);
        for (slot, &(_, v)) in key_values.iter_mut().zip(keys) {
            *slot = v;
        }
        // Safety: `claim` hands out an unpublished bucket this call owns.
        unsafe {
            (*bucket).discovered = 0;
            (*bucket).num_keys =
                u8::try_from(num_keys).expect("num_keys capped at MAX_EACH_KEYS=6");
            (*bucket).has_quality = has_quality;
            (*bucket).pass_count = 0;
            (*bucket).best_score = i64::MIN;
            (*bucket).key_values = key_values;
            (*bucket).msg = table::msg_buf(msg);
        }
    };
    // Safety: ptr was allocated with EACH_BUCKET_MEM_SIZE bytes.
    match unsafe { table::claim(ptr, (site_hash, bucket_hash), init) } {
        Claim::Entry(bucket) => Some(bucket),
        Claim::Busy | Claim::Full => None,
    }
}

/// Pack up to 4 quality key values into a single i64 for lexicographic comparison.
///
/// First key gets the highest 16 bits (highest priority).
/// Values are reduced to their low 16 bits (matching `v as u16` semantics) —
/// callers should pre-scale values into the u16 range if higher fidelity is needed.
fn pack_quality(quality: &[(&str, i64)]) -> i64 {
    let mut packed: i64 = 0;
    let n = quality.len().min(4);
    for (i, &(_, v)) in quality.iter().take(n).enumerate() {
        let shift = (3 - i) * 16;
        // Mask to low 16 bits (equivalent to `v as u16 as i64`, no sign loss).
        packed |= (v & 0xffff) << shift;
    }
    packed
}

/// Unpack a quality i64 back into individual values for display.
#[must_use]
pub fn unpack_quality(packed: i64, n: u8) -> Vec<i64> {
    (0..n as usize)
        .map(|i| {
            let shift = (3 - i) * 16;
            // Extract the low 16 bits at the shifted position (mirrors pack_quality).
            (packed >> shift) & 0xffff
        })
        .collect()
}

/// Backing function for per-value bucketed assertions.
///
/// Each unique combination of identity key values creates one bucket.
/// Signals discovery on first hit. Optional quality keys re-signal when the
/// packed quality score improves (CAS loop on `best_score`).
///
/// This is a no-op if `EachBucket` memory is not initialized.
pub fn assertion_sometimes_each(msg: &str, keys: &[(&str, i64)], quality: &[(&str, i64)]) {
    let site_hash = msg_hash(msg);
    let bucket_hash = bucket_hash(site_hash, keys);

    // `min(4)` guarantees the value fits in u8, so the cast is lossless.
    let has_quality = u8::try_from(quality.len().min(4)).unwrap_or(4);

    let Some(bucket) = find_or_alloc_each_bucket(site_hash, bucket_hash, keys, msg, has_quality)
    else {
        return;
    };

    // Safety: bucket points to valid memory. Atomic operations are used for
    // cross-process safety when concurrent worker processes share the region.
    unsafe {
        // Increment pass count.
        atomic(&raw const (*bucket).pass_count).fetch_add(1, Ordering::Relaxed);

        // Advance the quality watermark before publishing discovery. Using a
        // monotonic RMW here prevents a first-discovery worker from overwriting
        // a better score recorded concurrently by another worker.
        let quality_advanced = has_quality > 0 && {
            let score = pack_quality(quality);
            score > atomic(&raw const (*bucket).best_score).fetch_max(score, Ordering::Relaxed)
        };

        // Signal discovery on first hit: CAS discovered from 0 → 1.
        let first_discovery = atomic(&raw const (*bucket).discovered)
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();

        if first_discovery {
            on_discovery(DiscoveryKind::BucketFirst, bucket_hash);
        } else if quality_advanced {
            on_discovery(DiscoveryKind::BucketQuality, bucket_hash);
        }
    }
}

/// Read all recorded `EachBucket` entries.
///
/// Returns an empty vector if `EachBucket` memory is not initialized.
#[must_use]
pub fn each_bucket_read_all() -> Vec<EachBucket> {
    let ptr = crate::region::each_bucket_ptr();
    if ptr.is_null() {
        return Vec::new();
    }
    // Safety: ptr was allocated with EACH_BUCKET_MEM_SIZE bytes. Mutable
    // accounting fields are loaded atomically while immutable metadata is read
    // only after acquiring the publication latch.
    unsafe {
        table::published_entries::<EachBucket>(ptr)
            .map(|bucket| EachBucket {
                site_hash: (*bucket).site_hash,
                bucket_hash: (*bucket).bucket_hash,
                discovered: atomic(&raw const (*bucket).discovered).load(Ordering::Relaxed),
                num_keys: (*bucket).num_keys,
                has_quality: (*bucket).has_quality,
                published: table::READY,
                pass_count: atomic(&raw const (*bucket).pass_count).load(Ordering::Relaxed),
                best_score: atomic(&raw const (*bucket).best_score).load(Ordering::Relaxed),
                key_values: (*bucket).key_values,
                msg: (*bucket).msg,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_hash_is_a_stable_wire_format() {
        assert_eq!(
            bucket_hash(msg_hash("site"), &[("a", 1), ("b", -2)]),
            0x2a4a_985d_7d1d_1046
        );
    }

    #[test]
    fn test_pack_unpack_quality_roundtrip() {
        let quality = &[("health", 100i64), ("armor", 50i64), ("mana", 200i64)];
        let packed = pack_quality(quality);
        let unpacked = unpack_quality(packed, 3);
        assert_eq!(unpacked, vec![100, 50, 200]);
    }

    #[test]
    fn test_pack_quality_single() {
        let quality = &[("health", 42i64)];
        let packed = pack_quality(quality);
        let unpacked = unpack_quality(packed, 1);
        assert_eq!(unpacked, vec![42]);
    }

    #[test]
    fn test_each_bucket_size_stable() {
        // EachBucket must have a stable size for shared memory layout.
        // site_hash(8) + bucket_hash(8) + 1+1+1+1 + pass_count(4) +
        // best_score(8) + key_values(6*8) + msg(32) = 112 bytes
        assert_eq!(std::mem::size_of::<EachBucket>(), 112);
    }

    #[test]
    fn snapshots_skip_buckets_until_metadata_is_published() {
        crate::region::init();
        crate::region::reset();
        let buckets = crate::region::each_bucket_ptr();

        // Simulate an allocator that reserved index zero but has not finished
        // initializing its metadata. Readers must not expose the zeroed bucket.
        // Safety: `init` installed a correctly aligned bucket region.
        unsafe {
            table::header_word(buckets, 0).store(1, Ordering::Release);
        }
        assert!(each_bucket_read_all().is_empty());
        crate::region::clear();
    }

    #[test]
    fn test_each_bucket_read_all_when_inactive() {
        // Should return empty when not initialized.
        let buckets = each_bucket_read_all();
        assert!(buckets.is_empty());
    }

    #[test]
    fn test_assertion_sometimes_each_noop_when_inactive() {
        // Should not panic when EachBucket memory is not initialized.
        assertion_sometimes_each("test", &[("key", 1)], &[]);
    }

    #[test]
    fn test_quality_watermark_only_moves_forward() {
        crate::region::init();

        assertion_sometimes_each("quality", &[("key", 1)], &[("score", 10)]);
        assertion_sometimes_each("quality", &[("key", 1)], &[("score", 5)]);
        assertion_sometimes_each("quality", &[("key", 1)], &[("score", 20)]);

        let buckets = each_bucket_read_all();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].pass_count, 3);
        assert_eq!(unpack_quality(buckets[0].best_score, 1), vec![20]);

        crate::region::clear();
    }
}
