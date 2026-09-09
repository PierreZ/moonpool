//! I/O alignment: what an opened file requires, and buffers that satisfy it.
//!
//! Three separate numbers, deliberately not one:
//!
//! - **offset alignment** — where a transfer may start;
//! - **length alignment** — how large a transfer may be;
//! - **memory alignment** — where the caller's buffer may live.
//!
//! A buffered file constrains none of them ([`IoConstraints::NONE`]).
//! Direct I/O constrains all three, and a device may constrain them
//! differently, which is why they are reported separately rather than as one
//! "sector size".
//!
//! None of these is the database's block or page size, and none of them is a
//! crash-atomicity unit. A pager may use 16 KiB pages on a file whose transfer
//! alignment is 512 bytes and whose atomic write unit is neither.

use std::io;
use std::ops::{Deref, DerefMut};

/// The I/O alignment an opened file requires of its callers.
///
/// Obtained from [`StorageFile::constraints`](super::StorageFile::constraints).
/// Every value is a power of two, and `1` means "no constraint".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoConstraints {
    offset: u64,
    length: usize,
    memory: usize,
}

impl IoConstraints {
    /// No alignment requirement at all: ordinary buffered I/O.
    pub const NONE: Self = Self {
        offset: 1,
        length: 1,
        memory: 1,
    };

    /// Build constraints from three independent alignments.
    ///
    /// # Panics
    ///
    /// Panics unless every alignment is a power of two (`1` included).
    #[must_use]
    pub fn new(offset_alignment: u64, length_alignment: usize, memory_alignment: usize) -> Self {
        assert!(
            offset_alignment.is_power_of_two(),
            "offset alignment {offset_alignment} is not a power of two"
        );
        assert!(
            length_alignment.is_power_of_two(),
            "length alignment {length_alignment} is not a power of two"
        );
        assert!(
            memory_alignment.is_power_of_two(),
            "memory alignment {memory_alignment} is not a power of two"
        );
        Self {
            offset: offset_alignment,
            length: length_alignment,
            memory: memory_alignment,
        }
    }

    /// Build constraints where all three alignments are the same, the common
    /// case for a direct-I/O file on one device.
    ///
    /// # Panics
    ///
    /// Panics unless `alignment` is a power of two.
    #[must_use]
    pub fn uniform(alignment: usize) -> Self {
        Self::new(alignment as u64, alignment, alignment)
    }

    /// Alignment required of a transfer's starting offset.
    #[must_use]
    pub fn offset_alignment(&self) -> u64 {
        self.offset
    }

    /// Alignment required of a transfer's length.
    #[must_use]
    pub fn length_alignment(&self) -> usize {
        self.length
    }

    /// Alignment required of the caller's buffer address.
    #[must_use]
    pub fn memory_alignment(&self) -> usize {
        self.memory
    }

    /// Whether this file accepts any offset, length, and buffer.
    #[must_use]
    pub fn is_unconstrained(&self) -> bool {
        *self == Self::NONE
    }

    /// Check one transfer against these constraints.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] naming the alignment that was
    /// violated — the same class of failure a kernel returns for a misaligned
    /// `O_DIRECT` transfer.
    pub fn check(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.is_unconstrained() {
            return Ok(());
        }
        if !offset.is_multiple_of(self.offset) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "offset {offset} is not a multiple of the required offset alignment {}",
                    self.offset
                ),
            ));
        }
        if !buf.len().is_multiple_of(self.length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "transfer length {} is not a multiple of the required length alignment {}",
                    buf.len(),
                    self.length
                ),
            ));
        }
        if !(buf.as_ptr() as usize).is_multiple_of(self.memory) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "buffer address is not aligned to the required memory alignment {}",
                    self.memory
                ),
            ));
        }
        Ok(())
    }
}

impl Default for IoConstraints {
    fn default() -> Self {
        Self::NONE
    }
}

/// A zeroed byte buffer whose first byte sits on a chosen alignment boundary.
///
/// Direct I/O requires the caller's memory to be aligned, and a plain
/// `Vec<u8>` is aligned to one byte. This is the one aligned-buffer facility
/// in moonpool: layers above (a pager, [`BlockFile`](crate::BlockFile)) reuse
/// it rather than growing a second one. It is built from safe code by
/// over-allocating and slicing at the boundary.
///
/// ```
/// use moonpool_core::{AlignedBuf, IoConstraints};
///
/// let mut buf = AlignedBuf::for_constraints(4096, IoConstraints::uniform(4096));
/// buf.as_mut_slice()[0] = 1;
/// assert_eq!(buf.len(), 4096);
/// ```
#[derive(Debug)]
pub struct AlignedBuf {
    backing: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBuf {
    /// Allocate `len` zeroed bytes starting on an `alignment`-byte boundary.
    ///
    /// # Panics
    ///
    /// Panics unless `alignment` is a power of two, or if `len + alignment`
    /// does not fit in memory — the over-allocation must not wrap to a small
    /// buffer that would then be sliced out of bounds.
    #[must_use]
    pub fn zeroed(len: usize, alignment: usize) -> Self {
        assert!(
            alignment.is_power_of_two(),
            "alignment {alignment} is not a power of two"
        );
        let backing_len = len
            .checked_add(alignment)
            .expect("an aligned buffer of this length does not fit in memory");
        let backing = vec![0u8; backing_len];
        let offset = backing.as_ptr().align_offset(alignment);
        assert!(
            offset <= alignment,
            "an alignment boundary must exist within one alignment of the allocation"
        );
        Self {
            backing,
            offset,
            len,
        }
    }

    /// Allocate `len` zeroed bytes satisfying `constraints`' memory alignment.
    ///
    /// # Panics
    ///
    /// Panics if `len` does not satisfy the constraints' length alignment (a
    /// buffer that can never be transferred in one call is a caller bug, not a
    /// runtime condition), or if the allocation does not fit in memory. Block
    /// callers should reach for [`BlockFile::buffer`](crate::BlockFile::buffer),
    /// which reports the second case as an error instead.
    #[must_use]
    pub fn for_constraints(len: usize, constraints: IoConstraints) -> Self {
        assert!(
            len.is_multiple_of(constraints.length_alignment()),
            "buffer length {len} is not a multiple of the required length alignment {}",
            constraints.length_alignment()
        );
        Self::zeroed(len, constraints.memory_alignment())
    }

    /// The aligned bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.backing[self.offset..self.offset + self.len]
    }

    /// The aligned bytes, mutably.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.backing[self.offset..self.offset + self.len]
    }

    /// Number of usable bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer has no usable bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::{AlignedBuf, IoConstraints};

    #[test]
    fn unconstrained_accepts_everything() {
        let constraints = IoConstraints::NONE;
        assert!(constraints.is_unconstrained());
        assert!(constraints.check(7, &[0u8; 13]).is_ok());
    }

    #[test]
    fn each_alignment_is_checked_independently() {
        let constraints = IoConstraints::new(4096, 512, 1);
        let mut buf = AlignedBuf::zeroed(512, 1);

        assert!(constraints.check(4096, buf.as_mut_slice()).is_ok());
        assert!(
            constraints.check(512, buf.as_mut_slice()).is_err(),
            "an offset aligned to the length alignment is still a misaligned offset"
        );

        let mut short = AlignedBuf::zeroed(100, 1);
        assert!(constraints.check(0, short.as_mut_slice()).is_err());
    }

    #[test]
    fn aligned_buffers_satisfy_their_constraints() {
        let constraints = IoConstraints::uniform(4096);
        let buf = AlignedBuf::for_constraints(8192, constraints);
        assert_eq!(buf.len(), 8192);
        assert!(constraints.check(0, buf.as_slice()).is_ok());
        assert!(buf.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn misaligned_memory_is_rejected() {
        let constraints = IoConstraints::uniform(512);
        let buf = AlignedBuf::zeroed(1024, 512);
        // Slicing one aligned block off the front keeps the length legal but
        // moves the address off the boundary.
        assert!(constraints.check(0, &buf.as_slice()[1..513]).is_err());
    }
}
