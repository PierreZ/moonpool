//! Disk capacity: a simulated disk that fills up.
//!
//! A process's disk has no size limit by default. A harness gives one a
//! [`DiskCapacity`] with [`SimWorld::set_disk_capacity`](crate::SimWorld::set_disk_capacity)
//! (or [`FaultContext::set_disk_capacity`](crate::FaultContext::set_disk_capacity)).
//! From then on the disk counts the bytes its files use: the sum of every
//! file's length, an unlinked file still open included, times the
//! capacity's `amplification`. A write past the end of a file or a
//! `set_len` that grows one fails with [`std::io::ErrorKind::StorageFull`]
//! (`ENOSPC`) when the growth does not fit, and none of its bytes land. A
//! write inside a file's length never needs space, so a preallocated file
//! stays writable on a full disk. Deleting a file or shrinking one frees its
//! space at once.
//!
//! A full disk is a normal result, not chaos with a rate: the harness
//! decides which disk is small and when. Recovery mode
//! ([`SimWorld::enter_recovery_mode`](crate::SimWorld::enter_recovery_mode))
//! lifts every limit, so the quiet tail runs on disks with room. Setting a
//! capacity in recovery mode does nothing. The limit belongs to the machine:
//! it holds across a crash or a wipe of the process, whose files a wipe
//! deletes. Nothing here draws from the random stream.

use moonpool_core::DiskSpace;

/// The size limit of one process's simulated disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskCapacity {
    /// The disk's size: the bytes its files may use in all.
    pub bytes: u64,
    /// How many times the disk counts each byte of a file, at least 1.
    ///
    /// A value above 1 models a filesystem whose overhead (metadata, copy
    /// on write, a RAID mirror on the same budget) eats space faster than
    /// the data grows, and lets a harness fill a disk during a short run
    /// with an ordinary workload.
    pub amplification: u32,
}

impl DiskCapacity {
    /// A disk of `bytes` that counts each byte once.
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self {
            bytes,
            amplification: 1,
        }
    }

    /// The same disk counting each byte `amplification` times.
    ///
    /// # Panics
    ///
    /// Panics if `amplification` is zero.
    #[must_use]
    pub const fn amplified(self, amplification: u32) -> Self {
        assert!(amplification >= 1, "a disk counts each byte at least once");
        Self {
            bytes: self.bytes,
            amplification,
        }
    }

    /// The space `file_bytes` of file data uses on this disk.
    #[must_use]
    pub const fn used_by(self, file_bytes: u64) -> u64 {
        file_bytes.saturating_mul(self.amplification as u64)
    }

    /// The disk's space when its files hold `file_bytes` in all.
    #[must_use]
    pub const fn space(self, file_bytes: u64) -> DiskSpace {
        DiskSpace {
            total_bytes: self.bytes,
            free_bytes: self.bytes.saturating_sub(self.used_by(file_bytes)),
        }
    }

    /// Whether `growth` more bytes of file data fit when the files already
    /// hold `file_bytes`.
    #[must_use]
    pub const fn fits(self, file_bytes: u64, growth: u64) -> bool {
        self.used_by(file_bytes.saturating_add(growth)) <= self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_amplified_disk_fills_faster() {
        let plain = DiskCapacity::new(100);
        let fast = plain.amplified(3);
        assert!(plain.fits(50, 50));
        assert!(!plain.fits(50, 51));
        assert!(fast.fits(10, 23));
        assert!(!fast.fits(10, 24));
        assert_eq!(
            fast.space(30),
            DiskSpace {
                total_bytes: 100,
                free_bytes: 10
            }
        );
        assert_eq!(fast.space(40).free_bytes, 0);
    }
}
