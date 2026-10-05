//! A write-ahead journal over moonpool's [`BlockFile`](moonpool_core::BlockFile)
//! that tells a crash apart from corruption.
//!
//! The layout is CLSTORE, the local storage layer of the PAR/CTRL paper
//! (Alagappan et al., *Protocol-Aware Recovery for Consensus-Based Storage*,
//! FAST '18): every entry's identifier is stored in a slot **far from the
//! entry itself**. When an entry's checksum fails, its slot says whether the
//! entry was ever completely written — a torn write at the tail, which a
//! crash explains — or was written, acknowledged, and later damaged, which
//! only corruption explains. And it says exactly which entry is bad — its
//! index, its epoch, and the caller's own identity [`Tag`] — so a
//! replication layer can fetch it again.
//!
//! Because it is written against the provider traits only, the same journal
//! runs over `TokioStorageProvider` in production and over the simulator's
//! storage in tests.
//!
//! # Segments
//!
//! Each segment is one preallocated, zero-filled file named after its first
//! index (`seg-00000000000001048576.wal`, 64 MiB). The journal finds its
//! segments by listing the directory
//! ([`StorageProvider::list_dir`](moonpool_core::StorageProvider::list_dir)):
//! the names alone give their order.
//!
//! ```text
//! Offset   Region        Contents
//! 0 KiB    Header A      magic, version, first_index,
//! 4 KiB    Header B      slot_count, data_start, crc
//! 8 KiB    Slot table    32,768 slots × 64 B (2 MiB)
//! ~2 MiB   Guard gap     zeros (keeps IDs ≥2 MiB away)
//! 4 MiB    Data region   append-only entries → end
//! ```
//!
//! The header is written once, when the segment is created, and kept in two
//! copies. A segment rolls over when either its slot table or its data
//! region fills. The [`Geometry`] is configurable; the defaults are the sizes
//! above.
//!
//! Slot *i* lives at `8 KiB + 64 × (i − first_index)`:
//!
//! ```text
//! Slot (64 B)                  Entry (8-byte aligned)
//!  0  u64 index                 0  u32 magic
//!  8  u64 epoch                 4  u32 length
//! 16  u32 offset               8  u64 index
//! 20  u32 length              16  u64 epoch
//! 24  u32 entry_crc           24  u32 crc  (header+payload)
//! 28  u32 flags               28  u32 flags (batch start)
//! 32  tag (24 B)              32  tag (24 B)
//! 56  u32 reserved            56  u64 reserved
//! 60  u32 slot_crc (0–59)     64  …   payload
//! ```
//!
//! The entry repeats its index, epoch, tag, and batch-start flag, so a stale
//! record left by a lost or misdirected write, which may still pass its CRC,
//! gets caught, and an intact entry can rebuild a lost slot whole. The
//! batch-start flag marks the first entry of each append batch (see
//! *Recovery*). Two more flags live in the slot alone: *reserved* — every
//! slot of a new segment is formatted with a reserved record, its own index
//! and a CRC over the lot (`TigerBeetle`'s reserved headers), so a slot that
//! holds no entry says so positively and all zeros is damage, never "empty";
//! a reserved record naming another index is misdirected — and *cut*, which
//! marks the last entry kept by a truncation (see *Truncation*).
//!
//! # Identity: index, epoch, and tag
//!
//! Raft identifies an entry by its index and term, and so does the slot. A
//! caller whose identity is wider puts the rest in the 24-byte [`Tag`] of
//! each [`Record`]: a Paxos acceptor journaling its accepts as a
//! write-ahead log of operations, for example, appends under a sequence
//! number and tags each record with the slot and the ballot it is for. The
//! tag lives in the slot, far from the entry, so when the entry's bytes are
//! lost the recovery report ([`Recovery::corrupt`], [`EntryId`]) still says
//! exactly which Paxos slot and ballot to repair from peers.
//!
//! # Appending
//!
//! Per batch: `pwrite` the entries contiguously into the data region,
//! `pwrite` their slots in one call, `fdatasync` once, then acknowledge.
//! Nothing orders the first write before the second: one sync per batch
//! instead of two.
//!
//! Every batch starts on a fresh block (the [`BLOCK`] after the last entry,
//! its last block padded with zeros), so the data write never rewrites a
//! sector an earlier sync made durable: no crash during an append can damage
//! an acknowledged entry, whatever the disk does to the sectors being
//! written. The cost is space, up to one block of padding per batch, never a
//! sync. The slot write does rewrite the acknowledged slots that share its
//! blocks of the slot table; a crash that loses one leaves its entry intact,
//! and recovery rebuilds the slot from it.
//!
//! # Recovery
//!
//! Opening walks the indexes in order. Entry *i*'s offset comes from slot
//! *i*, or, if that slot is damaged, from the end of entry *i − 1*: right
//! there for an entry that continues its batch, or on the next block for one
//! that starts a batch — the batch-start flag in the entry says which, and a
//! slot marked *cut* says nothing continues. An entry is *good* only if its
//! CRC passes and its index matches (and, with a valid slot, its epoch,
//! length and CRC agree with the slot).
//!
//! | Entry | Slot      | Action                                       |
//! |-------|-----------|----------------------------------------------|
//! | any   | reserved  | the log ends here                            |
//! | good  | valid     | keep                                         |
//! | good  | bad       | keep, rewrite slot                           |
//! | bad   | valid     | mark corrupt, report its identity upward     |
//! | bad   | bad       | in the last batch: torn tail, the log ends   |
//! | bad   | bad       | before it: double fault, refuse to start     |
//!
//! This is the paper's rule for batched appends: the first entry without an
//! identifier ends the log, and every earlier faulty entry with one is
//! corrupted. The *last batch* is the exception the paper proves
//! unavoidable (its Appendix A, stated there for the last entry): its
//! entries and slots reach the disk through two unordered writes and one
//! sync, so a crash before the sync returned can leave any of its entries
//! with an identifier beside bad bytes, just as corruption would — and no
//! later durable entry proves otherwise — or tear both copies of an
//! identity at once. Every entry and slot carries a batch-start flag, so
//! opening finds the last batch itself: an index is in it when no
//! identifier further on (before a reserved one) starts a batch. There a
//! damaged identifier beside a damaged entry ends the log as a torn tail;
//! anywhere earlier a later sync covers it, and it is a double fault. The
//! last batch's damaged entries with intact identifiers are reported in
//! [`Recovery::ambiguous_batch`], never in
//! [`Recovery::corrupt`], and [`JournalConfig::ambiguous_tail`] decides the
//! rest: a single node treats them as a torn write and truncates from the
//! first of them ([`AmbiguousTail::Truncate`], the default); a replicated
//! system keeps them, marked corrupt, for the replication layer to keep if
//! committed and discard if not ([`AmbiguousTail::Keep`]) — kept, their
//! identities survive every later crash, where a truncation would erase the
//! only local evidence. Corruption before the last batch fails loudly
//! instead of being silently truncated.
//!
//! An EIO is treated as the paper does: the block is zero-filled, so its
//! entries fail their checksums and are reported corrupt. An identifier the
//! medium cannot read counts as damaged.
//!
//! Recovery is a truncation, so it cleans up like one: past the end of the
//! log, the discarded slots get their reserved records back and are synced,
//! then the discarded entries' blocks are zeroed, before any append.
//!
//! # Reading one entry
//!
//! The slot table is the index. Log indexes are dense and slots are
//! fixed-size, so a slot's position is computed, never searched for. A read
//! binary-searches the sorted segments for the largest first index at or
//! below *i*, then reads `32 + length` bytes at the slot's offset and checks
//! the entry's CRC, that its index and epoch match the slot, and that its CRC
//! equals `entry_crc`. The startup scan already read every slot, so the slot
//! comes from memory. Any failed check is [`JournalError::Corrupt`].
//!
//! [`Journal::read_range`] is the replay a caller runs after opening: the
//! same checks over a range, read in large sequential transfers (entries are
//! packed back to back), with a corrupt entry reported in place by its
//! [`EntryId`] rather than ending the replay.
//!
//! # Truncation and metadata
//!
//! [`Journal::truncate_suffix`] (Raft's conflicting-suffix truncation) cleans
//! up before the next append: it resets the discarded slots to their
//! reserved records (marking the last kept slot *cut*), then zeroes the
//! discarded entries' blocks, syncing after each, so a crash can never leave
//! an old slot beside a new entry and make a harmless crash look like
//! corruption. The block holding the cut is not rewritten — it holds kept
//! entries, and rewriting it would expose them to a crash — so the discarded
//! bytes in it stay, inert behind their reserved slots; the cut mark keeps
//! them from passing for a continuation even if the reserved slot after the
//! cut is damaged later. [`Journal::truncate_prefix`] deletes whole
//! segments. It first records the new start durably, in its own two-copy
//! record (`start.0`, `start.1`), so a crash that undoes some of its unlinks
//! (each name resolves on its own) cannot leave a gap that reads as the end
//! of the log: opening deletes again any segment wholly below the recorded
//! start, and a segment missing between the start and the tail is
//! [`JournalError::SegmentGap`], never a truncation. A sealed segment must
//! reach the next one's first index exactly, and the walk's bound comes
//! from the slot table, with the next segment's name only a cross-check.
//!
//! The caller's term/vote metadata lives in its own file, in two copies
//! (`meta.0`, `meta.1`), each with a generation counter and a CRC, each
//! updated via a temporary file, an fsync, and a rename. Loading uses the
//! valid copy with the highest generation and rewrites the other copy from
//! it when that one is damaged, missing, or a generation behind (a crash
//! between the two copy writes): left alone, a later fault in the newer copy
//! would roll the metadata back to a value the caller may already have acted
//! past ([`Recovery::meta_repaired`]). A save spends its generation before
//! writing either copy, even if it then fails, so a retry always outranks
//! what the failed save left behind; two valid copies of one generation
//! that disagree are damage, and opening refuses them
//! ([`JournalError::MetadataCorrupt`]).
//! [`Journal::peek_meta`] reads the metadata of a closed journal without
//! opening it: no recovery, no repair, nothing created.
//!
//! # The fault model
//!
//! The design assumes what the paper assumes of the disk: a crash leaves each
//! unsynced sector with its old contents or its new ones. The simulator can
//! also model harsher disks, where a crash destroys a sector that was being
//! rewritten even though a sync had made its old contents durable
//! (`FoundationDB`'s garbled in-flight pages). The journal never rewrites a
//! sector holding an acknowledged entry — not to append, not to truncate,
//! not to clean up — so on such a disk too a crash leaves every acknowledged
//! entry intact. What it does rewrite is slot-table blocks, which hold
//! acknowledged identifiers beside the ones being written; a crash that
//! loses one costs nothing, since recovery rebuilds it from its entry. The
//! crate's crash loops hold both models to the same promise.
//!
//! Physical separation of slots and entries is best-effort: the filesystem
//! controls block placement, though contiguous preallocated extents usually
//! keep the gap real.

mod dual;
mod error;
mod journal;
mod journal_atlas;
mod layout;
mod scan;
mod segment;

pub use error::JournalError;
pub use journal::{AmbiguousTail, Journal, JournalConfig, Record, Recovery};
pub use journal_atlas::{ChartedRegion, JournalAtlas, JournalRegion};
pub use layout::{BLOCK, ENTRY_HEADER_SIZE, Geometry, SLOT_SIZE, TAG_SIZE, Tag};
pub use segment::{Entry, EntryId};
