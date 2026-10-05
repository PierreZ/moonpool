//! What can go wrong, split into operating errors and evidence of damage.

use std::io;

use crate::EntryId;

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

    /// An entry is damaged while its slot is intact: the log knows exactly
    /// which entry is bad — its index, epoch, and tag, as the slot records
    /// them — and the replication layer can re-fetch it.
    #[error("entry {} (epoch {}) is corrupt", .0.index, .0.epoch)]
    Corrupt(EntryId),

    /// Both the entry at `index` and its slot are damaged, so nothing
    /// identifies what was there — CLSTORE crashes the node rather than
    /// guess. The journal refuses to start.
    #[error("double fault at index {index}: entry and slot are both damaged")]
    DoubleFault {
        /// The index whose entry and slot are both unusable.
        index: u64,
    },

    /// Neither header copy of a segment checks out, or it disagrees with the
    /// journal's configuration or name.
    #[error("segment starting at {first_index} has no valid header")]
    BadSegmentHeader {
        /// The segment's first index, from its name.
        first_index: u64,
    },

    /// The log ends at `end` inside a sealed segment, but the next segment
    /// starts at `next`: a segment, or the end of one, is missing. Segments
    /// are only ever removed whole, from either end, so a hole between the
    /// start and the tail is damage — never the end of the log — and
    /// opening refuses rather than discard what follows it.
    #[error("the log ends at {end} but the next segment starts at {next}")]
    SegmentGap {
        /// The index the log reaches before the hole.
        end: u64,
        /// The first index of the segment after it.
        next: u64,
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

    /// The two-copy metadata file exists but neither copy is valid, or
    /// both are valid at one generation with different payloads.
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

    /// An entry is larger than one segment's data region can hold.
    #[error("entry of {len} bytes exceeds the {max}-byte limit")]
    EntryTooLarge {
        /// The payload length.
        len: usize,
        /// The largest payload accepted.
        max: u64,
    },

    /// A checkpoint is one batch with one sync, so it must fit an empty
    /// segment's slot table and data region. Nothing was written.
    #[error("a checkpoint of {entries} entries ({bytes} bytes) does not fit one segment")]
    CheckpointTooLarge {
        /// The entries asked for.
        entries: usize,
        /// Their size on disk, headers and padding included.
        bytes: u64,
    },

    /// An earlier write failed, so the on-disk state is no longer known to
    /// match memory. Reopen the journal.
    #[error("journal is poisoned by an earlier write failure; reopen it")]
    Poisoned,
}
