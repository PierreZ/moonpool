//! A write-ahead journal over moonpool's [`BlockFile`](moonpool_core::BlockFile)
//! that tells a crash apart from corruption.
//!
//! The layout is CLSTORE, the local storage layer of the PAR/CTRL paper
//! (Alagappan et al., *Protocol-Aware Recovery for Consensus-Based Storage*,
//! FAST '18): every entry's identifier is stored in a slot **far from the
//! entry itself**. When an entry's checksum fails, its slot says whether the
//! entry was ever completely written — a torn write at the tail, which a
//! crash explains — or was written, acknowledged, and later damaged, which
//! only corruption explains. And it says exactly which entry is bad, and
//! which epoch it carried, so a replication layer can fetch it again.
//!
//! Because it is written against the provider traits only, the same journal
//! runs over `TokioStorageProvider` in production and over the simulator's
//! storage in tests.
//!
//! # Segments
//!
//! Each segment is one preallocated, zero-filled file named after its first
//! index (`seg-00000000001048576.wal`, 64 MiB). The journal finds its
//! segments by listing the directory
//! ([`StorageProvider::list_dir`](moonpool_core::StorageProvider::list_dir)):
//! the names alone give their order.
//!
//! ```text
//! Offset   Region        Contents
//! 0 KiB    Header A      magic, version, first_index,
//! 4 KiB    Header B      slot_count, data_start, crc
//! 8 KiB    Slot table    65,536 slots × 32 B (2 MiB)
//! ~2 MiB   Guard gap     zeros (keeps IDs ≥2 MiB away)
//! 4 MiB    Data region   append-only entries → end
//! ```
//!
//! The header is written once, when the segment is created, and kept in two
//! copies. A segment rolls over when either its slot table or its data
//! region fills. The [`Geometry`] is configurable; the defaults are the sizes
//! above.
//!
//! Slot *i* lives at `8 KiB + 32 × (i − first_index)`:
//!
//! ```text
//! Slot (32 B)                  Entry (8-byte aligned)
//!  0  u64 index                 0  u32 magic
//!  8  u64 epoch                 4  u32 length
//! 16  u32 offset               8  u64 index
//! 20  u32 length              16  u64 epoch
//! 24  u32 entry_crc           24  u32 crc  (header+payload)
//! 28  u32 slot_crc (0–27)     28  u32 reserved
//!                             32  …   payload
//! ```
//!
//! The entry repeats its index and epoch, so a stale record left by a lost
//! or misdirected write, which may still pass its CRC, gets caught.
//!
//! # Appending
//!
//! Per batch: `pwrite` the entries contiguously into the data region,
//! `pwrite` their slots in one call, `fdatasync` once, then acknowledge.
//! Nothing orders the first write before the second: one sync per batch
//! instead of two. `BlockFile` transfers whole blocks, so the block a batch
//! starts in is rewritten with the bytes it already held, from an in-memory
//! copy.
//!
//! # Recovery
//!
//! Opening walks the indexes in order. Entry *i*'s offset comes from slot
//! *i*, or, if that slot is unusable, from the end of entry *i − 1*. A slot is
//! *empty* if all its bytes are zero. An entry is *good* only if its CRC
//! passes and its index matches (and, with a valid slot, its epoch, length
//! and CRC agree with the slot).
//!
//! | Entry | Slot      | Action                                     |
//! |-------|-----------|--------------------------------------------|
//! | good  | valid     | keep                                       |
//! | good  | empty/bad | keep, rewrite slot                         |
//! | bad   | valid     | mark corrupt, report (index, epoch) upward |
//! | bad   | empty     | torn tail: truncate here                   |
//! | bad   | bad       | double fault: refuse to start              |
//!
//! This is the paper's rule for batched appends: the first entry without an
//! identifier ends the log, and every earlier faulty entry with one is
//! corrupted. The *last* entry is the exception the paper proves unavoidable
//! (its Appendix A): an identifier beside a bad entry at the very end is what
//! a crash between the slot write and the sync leaves, just as corruption
//! would. A replicated system hands it to the replication layer, which keeps
//! it if committed and discards it if not; this single-node journal treats it
//! as a torn write and reports it in [`Recovery::ambiguous_tail`]. Mid-log
//! corruption fails loudly instead of being silently truncated.
//!
//! An EIO is treated as the paper does: the block is zero-filled, so its
//! entries fail their checksums and are reported corrupt. An identifier the
//! medium cannot read counts as damaged rather than absent — zeros there
//! would otherwise pass for "no identifier".
//!
//! Recovery is a truncation, so it cleans up like one: past the end of the
//! log, the discarded slots are zeroed and synced, then the discarded entry
//! bytes, before any append — so "empty" keeps meaning all zeros.
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
//! # Truncation and metadata
//!
//! [`Journal::truncate_suffix`] (Raft's conflicting-suffix truncation) cleans
//! up before the next append: it zeroes the discarded slots, then the
//! discarded entries, syncing after each, so a crash can never leave an old
//! slot beside a new entry and make a harmless crash look like corruption.
//! Zeroing the slots first means a crash part-way leaves only entries
//! without identifiers past the cut: intact ones (kept, a prefix of the old
//! log) or zeroed ones (the end). [`Journal::truncate_prefix`] deletes whole
//! segments.
//!
//! The caller's term/vote metadata lives in its own file, in two copies
//! (`meta.0`, `meta.1`), each with a generation counter and a CRC, each
//! updated via a temporary file, an fsync, and a rename. Loading uses the
//! valid copy with the highest generation.
//!
//! # The fault model
//!
//! The design assumes what the paper assumes of the disk: a crash leaves each
//! unsynced sector with its old contents or its new ones. Rewriting the
//! acknowledged bytes that share the block a batch starts in is then
//! harmless. The simulator can also model harsher disks, where a crash
//! destroys a sector that was being rewritten even though a sync had made
//! its old contents durable (`FoundationDB`'s garbled in-flight pages). There,
//! an acknowledged entry sharing that sector can come back damaged — and the
//! journal detects it, reporting it corrupt or refusing to start, but cannot
//! keep it. The crate's tests run both models.
//!
//! Physical separation of slots and entries is best-effort: the filesystem
//! controls block placement, though contiguous preallocated extents usually
//! keep the gap real.

mod dual;
mod error;
mod journal;
mod layout;
mod scan;
mod segment;

pub use error::JournalError;
pub use journal::{Journal, JournalConfig, Record, Recovery};
pub use layout::{BLOCK, ENTRY_HEADER_SIZE, Geometry, SLOT_SIZE};
pub use segment::Entry;
