//! A write-ahead journal over moonpool's [`BlockFile`](moonpool_core::BlockFile)
//! that tells a crash apart from corruption.
//!
//! The layout follows CLSTORE, the local storage layer of the PAR/CTRL paper
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
//! storage — torn sectors, lost writes, misdirected I/O — in tests.
//!
//! # Segments
//!
//! The log is a sequence of segments. Each is one preallocated, zero-filled
//! file named after its first index (`seg-00000000001048576.wal`):
//!
//! ```text
//! Offset   Region        Contents
//! 0 KiB    Header A      magic, version, first_index, slot_count,
//! 4 KiB    Header B      data_start, segment_size, crc
//! 8 KiB    Slot table    slot_count slots × 32 B (65,536 → 2 MiB)
//! ~2 MiB   Guard gap     zeros (keeps identifiers ≥ 2 MiB from entries)
//! 4 MiB    Data region   append-only entries → end (64 MiB)
//! ```
//!
//! The header is written once, when the segment is created, in two copies. A
//! segment rolls over when its slot table or its data region fills. The
//! [`Geometry`] is configurable; the defaults are the sizes above.
//!
//! Slot *i* lives at `8 KiB + 32 × (i − first_index)`:
//!
//! ```text
//! Slot (32 B)                  Entry (8-byte aligned)
//!  0  u64 index                 0  u32 magic
//!  8  u64 epoch                 4  u32 length
//! 16  u32 offset               8  u64 index
//! 20  u32 length              16  u64 epoch
//! 24  u32 entry_crc           24  u32 crc  (header + payload)
//! 28  u32 slot_crc (0–27)     28  u32 position in its batch
//!                             32  …   payload
//! ```
//!
//! The entry repeats its index and epoch, so a stale record left behind by a
//! lost or misdirected write — which may well still pass its own CRC — is
//! caught by disagreeing with its slot.
//!
//! # Appending
//!
//! Per batch: write the entries contiguously into the data region, write
//! their slots in one call, `fdatasync` once, acknowledge. Nothing orders the
//! first write before the second — that is the trade: one sync per batch
//! instead of two, with recovery doing the disambiguation.
//!
//! Every batch starts on a [`BLOCK`] boundary. `BlockFile` transfers whole
//! blocks, and a batch that shared a block with the previous one would
//! rewrite already-acknowledged bytes without a sync covering them; a crash
//! could then tear an acknowledged entry. Aligning batches costs at most one
//! block of padding per batch and means an append never touches a block that
//! holds acknowledged entries. (Slot-table blocks *are* shared across
//! batches; a slot damaged that way is exactly the "good entry, bad slot"
//! case recovery repairs.)
//!
//! # Recovery
//!
//! Opening a journal walks every index in order. Entry *i* is located through
//! slot *i*, or — when that slot is unusable — from the end of entry *i − 1*
//! (or the next block boundary, where a new batch would have started). A slot
//! is *empty* when all its bytes are zero; an entry is *good* only when its
//! CRC passes and its index matches (and, with a valid slot, its epoch,
//! length and CRC agree with the slot).
//!
//! | Entry | Slot      | Before the last batch             | In the last batch            |
//! |-------|-----------|-----------------------------------|------------------------------|
//! | good  | valid     | keep                              | keep                         |
//! | good  | empty/bad | keep, rewrite the slot            | keep, rewrite the slot       |
//! | bad   | valid     | corrupt: report `(index, epoch)`  | ambiguous: truncate, report  |
//! | bad   | empty     | refuse: an entry went missing     | torn write: truncate         |
//! | bad   | bad       | refuse: double fault              | torn write: truncate         |
//!
//! The *last batch* is the one holding the last good entry (each entry
//! records its position in its batch), plus everything after it. Only one
//! batch is ever unsynced, and a crash resolves its sectors independently, so
//! a hole inside it is a crash; before it, every index was synced, so a hole
//! is damage. Truncation cuts at the first index in the last batch that is
//! not good. A single-node
//! journal cannot resolve the ambiguous last entry (slot present, entry bad),
//! so it treats it as torn and lists it in [`Recovery::ambiguous_tail`] for a
//! replication layer that can: keep it if it was committed, discard it if
//! not. Mid-log corruption fails loudly instead of being silently truncated.
//!
//! Before the journal accepts an append, recovery also zeroes whatever the
//! crashed incarnation may have written past the last kept entry — up to
//! [`JournalConfig::max_batch_bytes`], the most one unsynced batch can span —
//! so a stale entry that still passes its CRC can never be picked up later.
//!
//! # Reading
//!
//! Log indexes are dense and slots are fixed-size, so the slot table is a
//! plain array: a slot's position is computed, never searched for. The
//! startup scan has already verified every slot, so reads use the in-memory
//! copy, binary-search the segment list, read the entry, and check its CRC,
//! its index, and its agreement with the slot. Any failure is
//! [`JournalError::Corrupt`].
//!
//! # Truncation and metadata
//!
//! [`Journal::truncate_suffix`] (Raft's conflicting-suffix truncation)
//! zeroes the discarded slots and entries and syncs *before* returning, so a
//! crash can never leave an old slot beside a new entry and make a harmless
//! crash look like corruption. Zeroing is many writes, though, and a crash
//! resolves each on its own — half-zeroed slots beside surviving entries look
//! exactly like mid-log damage. So the cut is first made durable as a
//! *fence* in the manifest, and recovery discards everything at or past a
//! fence in whatever state it finds it, then scrubs the rest of that segment.
//! [`Journal::truncate_prefix`] drops whole segments below the new start.
//!
//! The storage provider cannot list a directory, so the segment list lives in
//! a manifest. It and the caller's own metadata ([`Journal::save_meta`], for
//! a term and a vote) are each kept in two copies (`<name>.0`, `<name>.1`)
//! with a generation counter and a CRC, each updated through a temporary
//! file, a sync, and a rename; loading takes the valid copy with the highest
//! generation. A segment is created — zero-filled, both headers written,
//! synced, its name made durable — before the manifest names it, and the
//! segment it follows is fully synced by then, so a sealed segment has no
//! tail.
//!
//! # Limits
//!
//! - Physical separation of slots and entries is best-effort: the filesystem
//!   decides block placement, though a contiguous preallocated file usually
//!   keeps the guard gap real.
//! - The last batch is inherently ambiguous: if it was synced and then an
//!   entry in it rotted, a single node cannot tell that from the batch being
//!   torn by a crash, and truncates it (reporting what it can in
//!   [`Recovery::ambiguous_tail`]).
//! - A suffix truncation that cuts inside a batch rewrites the block where the
//!   cut falls; see [`Journal::truncate_suffix`].
//! - When an entry and its slot are both damaged, recovery looks for a later
//!   intact entry within one [`JournalConfig::max_batch_bytes`] (probing block
//!   boundaries, where batches start) or through later valid slots. Finding
//!   one proves the damage is mid-log and the journal refuses to start; a
//!   double fault whose successors are all out of that reach, with their
//!   slots damaged too, reads as a torn tail.

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
