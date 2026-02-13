//! Per-value bucketed assertion tracking for `SOMETIMES_EACH` assertions.
//!
//! Each unique combination of identity key values creates one bucket.
//! On first discovery, triggers a fork to explore alternate timelines.
//! Optional quality watermarks re-fork when the packed quality score improves.

use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, Ordering};

/// Maximum number of each-buckets in shared memory.
pub const MAX_EACH_BUCKETS: usize = 256;

/// Maximum number of identity keys per bucket.
pub const MAX_EACH_KEYS: usize = 6;

const EACH_MSG_LEN: usize = 32;

/// One bucket's state in MAP_SHARED memory for SOMETIMES_EACH assertions.
/// Each unique combination of identity key values creates one bucket.
/// Optional quality watermark (`has_quality != 0`): re-forks when `best_score` improves.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EachBucket {
    /// FNV-1a hash of the assertion message string.
    pub site_hash: u32,
    /// FNV-1a hash incorporating identity key values.
    pub bucket_hash: u32,
    /// Whether a fork has been triggered for this bucket (0 = no, 1 = yes).
    pub fork_triggered: u8,
    /// Number of identity keys in this bucket.
    pub num_keys: u8,
    /// Number of quality keys (0 = no quality tracking).
    pub has_quality: u8,
    /// Padding for alignment.
    pub _pad: u8,
    /// Number of times this bucket has been hit.
    pub pass_count: u32,
    /// Best observed quality score (packed from quality key values).
    pub best_score: i64,
    /// Identity key values for this bucket.
    pub key_values: [i64; MAX_EACH_KEYS],
    /// Human-readable message for this bucket (null-terminated).
    pub msg: [u8; EACH_MSG_LEN],
}

impl EachBucket {
    /// Get the message string for this bucket.
    pub fn msg_str(&self) -> &str {
        let len = self
            .msg
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(EACH_MSG_LEN);
        std::str::from_utf8(&self.msg[..len]).unwrap_or("???")
    }
}

/// FNV-1a hash of a message string -> stable u32 mark ID.
fn msg_hash(msg: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in msg.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Layout: `[next_bucket: u32, _pad: u32, buckets: [EachBucket; MAX_EACH_BUCKETS]]`
///
/// Find or allocate an EachBucket by `(site_hash, bucket_hash)`.
fn find_or_alloc_each_bucket(
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

/// Compute 0-based index of an EachBucket from its pointer.
fn compute_each_bucket_index(base_ptr: *mut u8, bucket: *const EachBucket) -> usize {
    if base_ptr.is_null() {
        return 0;
    }
    let buckets_base = unsafe { base_ptr.add(8) } as usize;
    let offset = (bucket as usize).saturating_sub(buckets_base);
    offset / std::mem::size_of::<EachBucket>()
}

/// Pack quality key values into a single i64 for lexicographic comparison.
/// First key gets highest 16 bits (highest priority). Up to 4 quality keys.
pub fn pack_quality(quality: &[(&str, i64)]) -> i64 {
    let mut packed: i64 = 0;
    let n = quality.len().min(4);
    for (i, &(_, v)) in quality.iter().take(n).enumerate() {
        let shift = (3 - i) * 16;
        packed |= ((v as u16) as i64) << shift;
    }
    packed
}

/// Unpack quality i64 back into individual values for display.
pub fn unpack_quality(packed: i64, n: u8) -> Vec<i64> {
    (0..n as usize)
        .map(|i| {
            let shift = (3 - i) * 16;
            ((packed >> shift) as u16) as i64
        })
        .collect()
}

/// Backing function for SOMETIMES_EACH assertions.
/// Each unique combination of identity key values creates one bucket; forks on first
/// discovery. Optional quality keys re-fork when the packed quality score improves.
pub fn assertion_sometimes_each(msg: &str, keys: &[(&str, i64)], quality: &[(&str, i64)]) {
    let ptr = crate::context::EACH_BUCKET_PTR.with(|c| c.get());
    if ptr.is_null() {
        return;
    }

    // Compute bucket hash: site_hash mixed with identity key values only via FNV-1a.
    // Quality values are NOT included in the hash — they're watermarks, not identity keys.
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
    let bucket = find_or_alloc_each_bucket(ptr, site_hash, bucket_hash, keys, msg, has_quality);
    if bucket.is_null() {
        return;
    }

    unsafe {
        // Increment pass count.
        let count_atomic = &*((&(*bucket).pass_count) as *const u32 as *const AtomicU32);
        count_atomic.fetch_add(1, Ordering::Relaxed);

        // Fork on first discovery: CAS fork_triggered from 0 -> 1.
        let ft = &*((&(*bucket).fork_triggered) as *const u8 as *const AtomicU8);
        let first_discovery = ft
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();

        if first_discovery {
            // On first discovery, initialize best_score via atomic store if quality-tracked.
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
            // CAS loop on best_score.
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

/// Read all recorded SOMETIMES_EACH buckets from shared memory.
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

/// Total size in bytes of the each-bucket shared memory region.
pub const fn each_bucket_region_size() -> usize {
    8 + MAX_EACH_BUCKETS * std::mem::size_of::<EachBucket>()
}
