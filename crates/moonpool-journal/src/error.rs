//! What can go wrong, split into operating errors and evidence of damage.

use std::io;

/// Errors from opening, reading, or writing a [`Journal`](crate::Journal).
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// The storage provider failed. A write-side failure also poisons the
    /// journal: reopen it to recover.
    #[error("journal I/O failed: {0}")]
    Io(#[from] io::Error),

    /// The configuration cannot describe a journal.
    #[error("invalid journal configuration: {0}")]
    InvalidConfig(String),

    /// The entry at `index` is damaged while its slot is intact: the log
    /// knows exactly which entry is bad and which epoch it carried, and the
    /// replication layer can re-fetch it.
    #[error("entry {index} (epoch {epoch}) is corrupt")]
    Corrupt {
        /// The damaged entry's index.
        index: u64,
        /// The epoch its slot records.
        epoch: u64,
    },

    /// Both the entry at `index` and its slot are damaged, and a later entry
    /// is intact — so this is not a torn tail, and nothing identifies what was
    /// lost. The journal refuses to start.
    #[error("double fault at index {index}: entry and slot are both damaged mid-log")]
    DoubleFault {
        /// The index whose entry and slot are both unusable.
        index: u64,
    },

    /// The entry at `index` is damaged and its slot is empty, yet a later
    /// entry is intact: an acknowledged entry went missing. The journal
    /// refuses to start rather than truncate acknowledged data.
    #[error("entry {index} is missing mid-log (damaged entry, empty slot)")]
    MissingEntry {
        /// The index that cannot be found.
        index: u64,
    },

    /// Neither header copy of a segment checks out, or it disagrees with the
    /// journal's configuration or name.
    #[error("segment starting at {first_index} has no valid header")]
    BadSegmentHeader {
        /// The segment's first index, from its name.
        first_index: u64,
    },

    /// A segment file has the wrong size: segments are preallocated, so a
    /// size change is itself a fault.
    #[error("segment starting at {first_index} is {actual} bytes, expected {expected}")]
    SegmentSize {
        /// The segment's first index.
        first_index: u64,
        /// The configured segment size.
        expected: u64,
        /// The size found on disk.
        actual: u64,
    },

    /// The manifest names a segment that does not exist.
    #[error("segment starting at {first_index} is missing")]
    SegmentMissing {
        /// The missing segment's first index.
        first_index: u64,
    },

    /// A two-copy metadata file (`name` is `manifest` or `meta`) exists but
    /// neither copy is valid.
    #[error("both copies of {name} are damaged")]
    MetadataCorrupt {
        /// Which file.
        name: &'static str,
    },

    /// `index` is outside the live log `[start, next)`.
    #[error("index {index} is outside the journal's live range [{start}, {next})")]
    OutOfRange {
        /// The index asked for.
        index: u64,
        /// First live index.
        start: u64,
        /// The next index to be appended.
        next: u64,
    },

    /// An entry is larger than one batch or one segment's data region can
    /// hold.
    #[error("entry of {len} bytes exceeds the {max}-byte limit")]
    EntryTooLarge {
        /// The payload length.
        len: usize,
        /// The largest payload accepted.
        max: u64,
    },

    /// An earlier write failed, so the on-disk state is no longer known to
    /// match memory. Reopen the journal.
    #[error("journal is poisoned by an earlier write failure; reopen it")]
    Poisoned,
}
