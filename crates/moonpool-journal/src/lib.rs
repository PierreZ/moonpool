//! A write-ahead journal that implements CLSTORE, the local storage layer
//! of *Protocol-Aware Recovery for Consensus-Based Storage* (Alagappan et
//! al., FAST '18): it **detects** damage, **disentangles** a crash from
//! corruption, and **identifies** what was damaged, so a replication layer
//! can repair it from peers instead of truncating it away.
//!
//! It is written against moonpool's provider traits only, so the same
//! journal runs over Tokio's filesystem in production and over the
//! simulator's disk in tests. It knows nothing of any protocol: an entry is
//! a `u64` **position** and an opaque [`Id`] the caller chooses (a Paxos
//! acceptor stores its ballot there, a Raft log its term).
//!
//! # The model
//!
//! - **Positions in any order.** [`Batch::put`] writes any position at or
//!   above the floor, in any order, with gaps, and overwrites: a lagging
//!   replica fills old positions while new ones arrive. Payloads go to an
//!   arrival-order entry log, so writes stay sequential whatever the order.
//! - **Tombstones and a floor.** [`Batch::clear`] removes a range (a Raft
//!   suffix truncation); [`Batch::truncate_prefix`] raises the floor, and
//!   segments holding nothing live are deleted from the front.
//! - **Metainfo.** The caller's node-unique state (a promise, a vote, a
//!   format marker) is up to [`META_MAX`] bytes in two local copies.
//! - **No snapshots and no compaction.** Space comes back through the floor
//!   and tombstones; CLSTORE's snapshot store is not part of this crate.
//!
//! # Files
//!
//! ```text
//! <dir>/meta                    metainfo copy A (block 0) and copy B (block 16)
//! <dir>/seg-<first batch>.wal   preallocated, zero-filled segments:
//!     header A | header B | persist log | guard gap | entry log
//! ```
//!
//! The **meta file** is separate because it is the one structure rewritten
//! in place and the one that must outlive every segment (`LogCabin`,
//! canonical/raft and hashicorp/raft-wal keep it apart too). Each **segment**
//! holds two append-only logs: 64-byte **persist records** (CLSTORE's
//! identifier and persist record in one: kind, position, id, the entry's
//! offset, length and CRC) and **entries** (a 64-byte header repeating the
//! record's identity, then the payload). Every batch starts on a fresh block
//! in both logs, so no write ever touches a sector holding anything
//! acknowledged. A segment header links to the last batch before it, so a
//! missing segment is never read as the end of the log.
//!
//! # Committing
//!
//! [`Durability`] picks the protocol, per batch:
//!
//! | Mode | Protocol | Syncs |
//! |---|---|---|
//! | [`Batched`](Durability::Batched) (the paper's) | `write(entries); write(records); fsync` | 1 |
//! | [`Ordered`](Durability::Ordered) | `write(entries); fsync; write(records); fsync` | 2 |
//!
//! Metainfo, when a batch changes it (with [`Batch::set_meta`] or a floor
//! raise), is written only **after** the batch's last sync: copy A, sync,
//! copy B, sync, so at most one copy is ever in flight. A crash can
//! therefore leave a batch durable without its metainfo, but never the
//! metainfo without its batch: a caller may put in the metainfo anything
//! that is true only once the batch's entries and tombstones are on disk.
//!
//! # Recovery
//!
//! Opening reads every persist record and judges each one (`last` is the
//! last batch in the log):
//!
//! | Entry | Persist record | Verdict |
//! |---|---|---|
//! | intact | valid | live |
//! | intact | damaged | live; the record is rebuilt from the entry |
//! | damaged | valid | [`State::Corrupt`], or [`State::Ambiguous`] in a `Batched` last batch |
//! | damaged | damaged, in `last` | torn by a crash: discarded |
//! | damaged | damaged, before `last` | [`OpenError::DoubleFault`]: refuse to open |
//!
//! A batch missing while later ones exist, or a log ending before the batch
//! the metainfo proves durable, is [`OpenError::LostBatch`]; a medium error
//! reads as damaged bytes (CLSTORE's treatment of EIO); a damaged or missing
//! file is a crash ([`OpenError::BadSegment`], [`OpenError::MetaLost`]).
//! Only the last batch's entries are read at open; the rest are checked
//! when read ([`Journal::read`], [`Journal::replay`]). What opening keeps is
//! synced before it returns.

mod batch;
mod error;
mod format;
mod io;
mod journal;
mod meta;
mod recover;
mod segment;

pub use batch::Batch;
pub use error::{CommitError, Durable, OpenError, ReadError};
pub use format::{BLOCK, Geometry, ID_SIZE, Id, MAX_BATCH_RECORDS, META_MAX, RECORD_SIZE};
pub use journal::{Durability, Entry, Journal, JournalConfig, JournalId, Layout, Recovery, State};
