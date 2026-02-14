//! Per-value bucketed exploration infrastructure for `assert_sometimes_each!`.
//!
//! Each unique combination of identity key values creates one bucket in shared
//! memory. On first discovery, a fork is triggered. Optional quality watermarks
//! allow re-forking when the packed quality score improves.
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

/// Maximum number of EachBucket slots in shared memory.
pub const MAX_EACH_BUCKETS: usize = 256;

/// Maximum number of identity keys per bucket.
pub const MAX_EACH_KEYS: usize = 6;

/// Maximum length of the assertion message stored in a bucket.
const EACH_MSG_LEN: usize = 32;

/// Total shared memory size for the EachBucket region.
pub const EACH_BUCKET_MEM_SIZE: usize = 8 + MAX_EACH_BUCKETS * std::mem::size_of::<EachBucket>();

/// One bucket's state in MAP_SHARED memory for per-value bucketed assertions.
///
/// Each unique combination of identity key values creates one bucket.
/// Optional quality watermark (`has_quality != 0`): re-forks when `best_score` improves.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EachBucket {
    /// FNV-1a hash of the assertion message string.
    pub site_hash: u32,
    /// Hash of (site_hash + identity key values) — uniquely identifies this bucket.
    pub bucket_hash: u32,
    /// CAS guard: 0 = no fork yet, 1 = first fork triggered.
    pub fork_triggered: u8,
    /// Number of identity keys stored in `key_values`.
    pub num_keys: u8,
    /// Number of quality keys (0-4). 0 means no quality tracking.
    pub has_quality: u8,
    /// Alignment padding.
    pub _pad: u8,
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
    pub fn msg_str(&self) -> &str {
        let len = self
            .msg
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(EACH_MSG_LEN);
        std::str::from_utf8(&self.msg[..len]).unwrap_or("???")
    }
}

/// FNV-1a hash of a message string to a stable u32.
fn msg_hash(msg: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in msg.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Find an existing bucket or allocate a new one by (site_hash, bucket_hash).
///
/// Returns a pointer to the bucket, or null if the table is full.
///
/// # Safety
///
/// `ptr` must point to a valid EachBucket shared memory region of at least
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
        let next_atomic = &*(ptr as *const AtomicU32);
        let count = next_atomic.load(Ordering::Relaxed) as usize;
        let base = ptr.add(8) as *mut EachBucket;

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
                fork_triggered: 0,
                num_keys: num_keys as u8,
                has_quality,
                _pad: 0,
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
fn pack_quality(quality: &[(&str, i64)]) -> i64 {
    let mut packed: i64 = 0;
    let n = quality.len().min(4);
    for (i, &(_, v)) in quality.iter().take(n).enumerate() {
        let shift = (3 - i) * 16;
        packed |= ((v as u16) as i64) << shift;
    }
    packed
}

/// Unpack a quality i64 back into individual values for display.
pub fn unpack_quality(packed: i64, n: u8) -> Vec<i64> {
    (0..n as usize)
        .map(|i| {
            let shift = (3 - i) * 16;
            ((packed >> shift) as u16) as i64
        })
        .collect()
}

/// Backing function for per-value bucketed assertions.
///
/// Each unique combination of identity key values creates one bucket.
/// Forks on first discovery. Optional quality keys re-fork when the packed
/// quality score improves (CAS loop on `best_score`).
///
/// This is a no-op if EachBucket shared memory is not initialized.
pub fn assertion_sometimes_each(msg: &str, keys: &[(&str, i64)], quality: &[(&str, i64)]) {
    let ptr = crate::context::EACH_BUCKET_PTR.with(|c| c.get());
    if ptr.is_null() {
        return;
    }

    // Compute bucket hash: site_hash mixed with identity key values only via FNV-1a.
    // Quality values are NOT included — they're watermarks, not identity keys.
    let site_hash = msg_hash(msg);
    let mut bucket_hash = site_hash;
    for &(_, val) in keys {
        for b in val.to_le_bytes() {
            bucket_hash ^= b as u32;
            bucket_hash = bucket_hash.wrapping_mul(0x01000193);
        }
    }

    let has_quality = quality.len().min(4) as u8;
    let score = if has_quality > 0 {
        pack_quality(quality)
    } else {
        0
    };

    // Safety: ptr was allocated during init() with EACH_BUCKET_MEM_SIZE bytes.
    let bucket =
        unsafe { find_or_alloc_each_bucket(ptr, site_hash, bucket_hash, keys, msg, has_quality) };
    if bucket.is_null() {
        return;
    }

    // Safety: bucket points to valid MAP_SHARED memory. Atomic operations are used
    // for cross-fork safety (parent waits on child via waitpid, but atomics ensure
    // correct visibility for recursive fork scenarios).
    unsafe {
        // Increment pass count.
        let count_atomic = &*((&(*bucket).pass_count) as *const u32 as *const AtomicU32);
        count_atomic.fetch_add(1, Ordering::Relaxed);

        // Fork on first discovery: CAS fork_triggered from 0 → 1.
        let ft = &*((&(*bucket).fork_triggered) as *const u8 as *const AtomicU8);
        let first_discovery = ft
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();

        if first_discovery {
            // On first discovery, initialize best_score if quality-tracked.
            if has_quality > 0 {
                let bs_atomic = &*((&(*bucket).best_score) as *const i64 as *const AtomicI64);
                bs_atomic.store(score, Ordering::Relaxed);
            }

            let bucket_index = compute_each_bucket_index(ptr, bucket);
            crate::fork_loop::dispatch_branch(
                msg,
                bucket_index % crate::assertion_slots::MAX_ASSERTION_SLOTS,
            );
        } else if has_quality > 0 {
            // Not first discovery: check quality watermark improvement.
            // CAS loop on best_score — re-fork when score improves.
            let bs_atomic = &*((&(*bucket).best_score) as *const i64 as *const AtomicI64);
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
                        crate::fork_loop::dispatch_branch(
                            msg,
                            bucket_index % crate::assertion_slots::MAX_ASSERTION_SLOTS,
                        );
                        break;
                    }
                    Err(actual) => current = actual,
                }
            }
        }
    }
}

/// Read all recorded EachBucket entries from shared memory.
///
/// Returns an empty vector if EachBucket shared memory is not initialized.
pub fn each_bucket_read_all() -> Vec<EachBucket> {
    let ptr = crate::context::EACH_BUCKET_PTR.with(|c| c.get());
    if ptr.is_null() {
        return Vec::new();
    }
    unsafe {
        let count = (*(ptr as *const u32)) as usize;
        let count = count.min(MAX_EACH_BUCKETS);
        let base = ptr.add(8) as *const EachBucket;
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
