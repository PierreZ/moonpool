//! What can go wrong: operating errors, and evidence of damage the journal
//! will not guess about.

use std::io;

/// Why a journal could not be created or opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The storage provider failed. Opening again is how a caller retries.
    #[error("journal I/O failed: {0}")]
    Io(#[from] io::Error),

    /// The configuration cannot describe a journal, or disagrees with the
    /// geometry the journal was created with.
    #[error("invalid journal configuration: {0}")]
    InvalidConfig(&'static str),

    /// `create` found a journal already there.
    #[error("a journal already exists in this directory")]
    AlreadyExists,

    /// A persist record and its entry are both damaged before the last
    /// batch: nothing identifies what was there, and a later batch proves
    /// it was synced. CLSTORE crashes rather than guess.
    #[error("double fault in batch {batch}: record {index} and its entry are both damaged")]
    DoubleFault {
        /// The batch holding the record.
        batch: u64,
        /// The record's index within its batch.
        index: u16,
    },

    /// A batch the log must contain is gone: a later batch exists without
    /// it, or the metainfo proves it was durable and the log ends before it.
    #[error("batch {batch} is missing")]
    LostBatch {
        /// The first batch missing.
        batch: u64,
    },

    /// Both metainfo copies are damaged, or the meta file is missing while
    /// segments exist. The metainfo is node-unique: no peer can restore it.
    #[error("both metainfo copies are lost")]
    MetaLost,

    /// A segment file is unusable at file granularity: wrong size, no valid
    /// header copy, or a header naming another journal or another batch.
    /// CLSTORE crashes on file-system metadata faults.
    #[error("segment {name} is unusable: {reason}")]
    BadSegment {
        /// The segment's file name.
        name: String,
        /// What is wrong with it.
        reason: &'static str,
    },

    /// The directory holds another journal's files.
    #[error("the directory holds journal {found:#x}")]
    WrongJournal {
        /// The identity found on disk.
        found: u128,
    },
}

/// Whether a failed write's effect may have reached the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durable {
    /// Nothing was written: the batch is known absent.
    No,
    /// Some write was issued: the batch may be durable anyway. Neither
    /// outcome may be assumed; reopen and read what the disk holds.
    Unknown,
}

/// Why a commit failed. A failed commit poisons the journal.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    /// A position below the floor (after this batch's own truncation).
    #[error("position {position} is below the floor {floor}")]
    BelowFloor {
        /// The position asked for.
        position: u64,
        /// The floor it is below.
        floor: u64,
    },
    /// The caller's metainfo exceeds [`META_MAX`](crate::META_MAX).
    #[error("metainfo of {len} bytes exceeds the limit")]
    MetaTooLarge {
        /// Its length.
        len: usize,
    },
    /// The batch cannot fit an empty segment, or holds more than
    /// [`MAX_BATCH_RECORDS`](crate::MAX_BATCH_RECORDS) records.
    #[error("the batch does not fit one segment")]
    BatchTooLarge,
    /// The storage provider failed.
    #[error("journal write failed ({durable:?}): {error}")]
    Io {
        /// The provider's error.
        error: io::Error,
        /// Whether the batch may be on disk.
        durable: Durable,
    },
    /// An earlier commit failed: memory no longer matches the disk. Reopen.
    #[error("the journal is poisoned by an earlier failure; reopen it")]
    Poisoned,
}

/// Why a read returned no bytes.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The storage provider failed in a way that is not the medium (a
    /// medium error reads as damage, CLSTORE's treatment of EIO).
    #[error("journal read failed: {0}")]
    Io(#[from] io::Error),
    /// Nothing live at this position.
    #[error("nothing live at position {position}")]
    Empty {
        /// The position asked for.
        position: u64,
    },
    /// The entry's bytes are damaged; its identity survived in its record.
    #[error("the entry at position {position} is damaged")]
    Damaged {
        /// The position.
        position: u64,
        /// The identity its record carries.
        id: crate::Id,
    },
}
