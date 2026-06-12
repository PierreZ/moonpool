//! Per-value bucketed accounting for `assert_sometimes_each!`.
//!
//! Each unique combination of identity key values creates one bucket. On first
//! discovery the accounting calls [`crate::hooks::on_bucket_split`]; on every
//! call it calls [`crate::hooks::on_bucket_mark`]. With no hook installed both
//! are no-ops (pure accounting); the exploration backend wires them to coverage
//! marking and fork dispatch. Optional quality watermarks allow re-signalling
//! when the packed quality score improves.
//!
//! # Memory Layout
//!
//! ```text
//! [next_bucket: u32, _pad: u32, buckets: [EachBucket; MAX_EACH_BUCKETS]]
//! ```
//!
//! The `next_bucket` counter is incremented atomically (via `AtomicU32::fetch_add`)
//! to allocate new buckets safely across fork boundaries.

use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, Ordering};

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
    /// FNV-1a hash of the assertion message string.
    pub site_hash: u32,
    /// Hash of (`site_hash` + identity key values) — uniquely identifies this bucket.
    pub bucket_hash: u32,
    /// CAS guard: 0 = no fork yet, 1 = first fork triggered.
    pub split_triggered: u8,
    /// Number of identity keys stored in `key_values`.
    pub num_keys: u8,
    /// Number of quality keys (0-4). 0 means no quality tracking.
    pub has_quality: u8,
    /// Alignment padding.
    pad: u8,
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
        let len = self
            .msg
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(EACH_MSG_LEN);
        std::str::from_utf8(&self.msg[..len]).unwrap_or("???")
    }
}

use crate::slots::msg_hash;

/// Find an existing bucket or allocate a new one by (`site_hash`, `bucket_hash`).
///
/// Returns a pointer to the bucket, or null if the table is full.
///
/// # Safety
///
/// `ptr` must point to a valid `EachBucket` memory region of at least
/// `EACH_BUCKET_MEM_SIZE` bytes.
unsafe fn find_or_alloc_each_bucket(
    ptr: *mut u8,
    site_hash: u32,
    bucket_hash: u32,
    keys: &[(&str, i64)],
    msg: &str,
    has_quality: u8,
) -> *mut EachBucket {
    unsafe {
        let next_atomic = &*ptr.cast::<()>().cast::<AtomicU32>();
        let count = next_atomic.load(Ordering::Relaxed) as usize;
        let base = ptr.add(8).cast::<()>().cast::<EachBucket>();

        // Search existing buckets.
        for i in 0..count.min(MAX_EACH_BUCKETS) {
            let bucket = base.add(i);
            if (*bucket).site_hash == site_hash && (*bucket).bucket_hash == bucket_hash {
                return bucket;
            }
        }

        // Allocate new bucket atomically.
        let new_idx = next_atomic.fetch_add(1, Ordering::Relaxed) as usize;
        if new_idx >= MAX_EACH_BUCKETS {
            next_atomic.fetch_sub(1, Ordering::Relaxed);
            return std::ptr::null_mut();
        }

        let bucket = base.add(new_idx);
        let mut msg_buf = [0u8; EACH_MSG_LEN];
        let n = msg.len().min(EACH_MSG_LEN - 1);
        msg_buf[..n].copy_from_slice(&msg.as_bytes()[..n]);

        let mut key_values = [0i64; MAX_EACH_KEYS];
        let num_keys = keys.len().min(MAX_EACH_KEYS);
        for (i, &(_, v)) in keys.iter().take(num_keys).enumerate() {
            key_values[i] = v;
        }

        std::ptr::write(
            bucket,
            EachBucket {
                site_hash,
                bucket_hash,
                split_triggered: 0,
                num_keys: u8::try_from(num_keys).expect("num_keys capped at MAX_EACH_KEYS=6"),
                has_quality,
                pad: 0,
                pass_count: 0,
                best_score: i64::MIN,
                key_values,
                msg: msg_buf,
            },
        );
        bucket
    }
}

/// Compute the 0-based array index of a bucket from its pointer.
fn compute_each_bucket_index(base_ptr: *mut u8, bucket: *const EachBucket) -> usize {
    if base_ptr.is_null() {
        return 0;
    }
    let buckets_base = unsafe { base_ptr.add(8) } as usize;
    let offset = (bucket as usize).saturating_sub(buckets_base);
    offset / std::mem::size_of::<EachBucket>()
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
    let ptr = crate::region::each_bucket_ptr();
    if ptr.is_null() {
        return;
    }

    // Compute bucket hash: site_hash mixed with identity key values only via FNV-1a.
    // Quality values are NOT included — they're watermarks, not identity keys.
    let site_hash = msg_hash(msg);
    let mut bucket_hash = site_hash;
    for &(_, val) in keys {
        for b in val.to_le_bytes() {
            bucket_hash ^= u32::from(b);
            bucket_hash = bucket_hash.wrapping_mul(0x0100_0193);
        }
    }

    // Mark coverage for adaptive yield detection (on every call). Different
    // identity key combinations produce different bucket_hash values, so the
    // coverage map distinguishes e.g. floor-1 from floor-2 assertions.
    crate::hooks::on_bucket_mark(bucket_hash);

    // `min(4)` guarantees the value fits in u8, so the cast is lossless.
    let has_quality = u8::try_from(quality.len().min(4)).unwrap_or(4);
    let score = if has_quality > 0 {
        pack_quality(quality)
    } else {
        0
    };

    // Safety: ptr was allocated with EACH_BUCKET_MEM_SIZE bytes.
    let bucket =
        unsafe { find_or_alloc_each_bucket(ptr, site_hash, bucket_hash, keys, msg, has_quality) };
    if bucket.is_null() {
        return;
    }

    // Safety: bucket points to valid memory. Atomic operations are used for
    // cross-fork safety (parent waits on child via waitpid, but atomics ensure
    // correct visibility for recursive fork scenarios).
    unsafe {
        // Increment pass count.
        let count_atomic = &*(&raw const (*bucket).pass_count).cast::<AtomicU32>();
        count_atomic.fetch_add(1, Ordering::Relaxed);

        // Signal discovery on first hit: CAS split_triggered from 0 → 1.
        let ft = &*(&raw const (*bucket).split_triggered).cast::<AtomicU8>();
        let first_discovery = ft
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();

        if first_discovery {
            // On first discovery, initialize best_score if quality-tracked.
            if has_quality > 0 {
                let bs_atomic = &*(&raw const (*bucket).best_score).cast::<AtomicI64>();
                bs_atomic.store(score, Ordering::Relaxed);
            }

            let bucket_index = compute_each_bucket_index(ptr, bucket);
            crate::hooks::on_bucket_split(msg, bucket_index % crate::slots::MAX_ASSERTION_SLOTS);
        } else if has_quality > 0 {
            // Not first discovery: check quality watermark improvement.
            // CAS loop on best_score — re-signal when score improves.
            let bs_atomic = &*(&raw const (*bucket).best_score).cast::<AtomicI64>();
            let mut current = bs_atomic.load(Ordering::Relaxed);
            loop {
                if score <= current {
                    break;
                }
                match bs_atomic.compare_exchange_weak(
                    current,
                    score,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let bucket_index = compute_each_bucket_index(ptr, bucket);
                        crate::hooks::on_bucket_split(
                            msg,
                            bucket_index % crate::slots::MAX_ASSERTION_SLOTS,
                        );
                        break;
                    }
                    Err(actual) => current = actual,
                }
            }
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
    // Safety: ptr was allocated with EACH_BUCKET_MEM_SIZE bytes.
    // - The first 4 bytes hold the bucket count (u32), capped at MAX_EACH_BUCKETS.
    // - base = ptr + 8 is the start of the EachBucket array.
    // - Loop bound 0..count ensures base.add(i) stays within the allocated region.
    // - EachBucket is #[repr(C)] + Copy, so ptr::read is valid for initialized slots.
    unsafe {
        let count = (*ptr.cast::<()>().cast::<u32>()) as usize;
        let count = count.min(MAX_EACH_BUCKETS);
        let base = ptr.add(8).cast::<()>().cast::<EachBucket>();
        (0..count).map(|i| std::ptr::read(base.add(i))).collect()
    }
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
    fn test_msg_hash_different_inputs() {
        let h1 = msg_hash("alpha");
        let h2 = msg_hash("beta");
        let h3 = msg_hash("gamma");
        assert_ne!(h1, h2);
        assert_ne!(h2, h3);
        assert_ne!(h1, h3);
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
        // 4+4+1+1+1+1+4+8+6*8+32 = 104 bytes
        assert_eq!(std::mem::size_of::<EachBucket>(), 104);
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
}
