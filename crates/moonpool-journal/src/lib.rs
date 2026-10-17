//! A write-ahead journal that implements CLSTORE, the local storage layer of
//! *Protocol-Aware Recovery for Consensus-Based Storage* (Alagappan et al.,
//! FAST '18).
//!
//! The journal **detects** damage, **tells a crash apart from corruption**,
//! and **names** what was damaged. A replication layer can then repair the
//! damaged entry from its peers. It does not have to truncate the log.
//!
//! A plain log that finds a bad checksum usually truncates there. That is
//! correct for a torn write at the tail. It is wrong for an acknowledged
//! entry that rotted: truncation can erase committed data across the whole
//! cluster. CLSTORE keeps the identity of every entry in a persist record
//! far from the entry. When the entry fails its checksum, the record proves
//! that it was written and says which entry it was.
//!
//! The journal uses only moonpool's provider traits. The same code runs over
//! `TokioStorageProvider` in production and over the simulator's
//! `SimStorageProvider` in tests. It knows no protocol: an entry is a `u64`
//! **position** and an opaque [`Id`] that the caller chooses. A Paxos
//! acceptor stores its ballot in the [`Id`], a Raft log stores its term.
//!
//! # Quick example
//!
//! Create a journal, commit one batch, reopen it and read the entry back.
//!
//! ```rust
//! use moonpool_core::TokioStorageProvider;
//! use moonpool_journal::{Batch, ID_SIZE, Journal, JournalConfig, JournalId, State};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let root = std::env::temp_dir().join(format!("moonpool-journal-doc-{}", std::process::id()));
//!     let dir = root.join("wal");
//!     let dir = dir.to_str().ok_or("path is not UTF-8")?;
//!     let provider = TokioStorageProvider::new();
//!     let journal_id = JournalId(7);
//!
//!     // `create` writes the first metainfo; `open` returns `None` when
//!     // there is no journal in the directory.
//!     let mut journal =
//!         Journal::create(provider.clone(), dir, journal_id, JournalConfig::default(), b"vote=none")
//!             .await?;
//!
//!     // Positions go in any order. The metainfo lands after the entries.
//!     let ballot = [3_u8; ID_SIZE];
//!     let mut batch = Batch::new();
//!     batch.put(9, ballot, "set y=9").put(4, ballot, "set x=4").set_meta("vote=3");
//!     journal.commit(batch).await?;
//!     drop(journal);
//!
//!     // Opening runs recovery and reports what it found and repaired.
//!     let (journal, recovery) = Journal::open(provider, dir, journal_id, JournalConfig::default())
//!         .await?
//!         .ok_or("the journal is gone")?;
//!     assert!(recovery.corrupt.is_empty());
//!     assert_eq!(journal.meta(), b"vote=3");
//!     assert!(matches!(journal.state(4), State::Live { .. }));
//!     assert_eq!(journal.read(9).await?.payload, b"set y=9");
//!
//!     std::fs::remove_dir_all(&root)?;
//!     Ok(())
//! }
//! ```
//!
//! # Features
//!
//! - **Positions in any order.** [`Batch::put`] writes any position at or
//!   above the floor, in any order, with gaps and overwrites. A lagging
//!   replica fills old positions while new ones arrive. Payloads go to an
//!   arrival-order entry log, so the writes stay sequential.
//! - **Tombstones and a floor.** [`Batch::clear`] removes a range (a Raft
//!   suffix truncation). [`Batch::truncate_prefix`] raises the floor, and the
//!   journal deletes from the front the segments that hold nothing live.
//! - **Metainfo.** The caller's node-unique state (a promise, a vote, a
//!   format marker) is up to [`META_MAX`] bytes, kept in two local copies.
//!   [`Journal::peek_meta`] reads it from a closed journal without recovery.
//! - **Bounded batches.** [`Batch::fits`] and [`Batch::fits_another`] tell
//!   the caller whether a batch fits one segment of a [`Geometry`], so it can
//!   split its writes across commits before [`Journal::commit`] refuses them.
//! - **Two commit protocols.** [`Durability::Batched`] (one sync) and
//!   [`Durability::Ordered`] (two syncs), chosen per open.
//! - **Damage is evidence.** [`State::Corrupt`], [`State::Ambiguous`],
//!   [`ReadError::Damaged`] and the evidence variants of [`OpenError`] report
//!   damage with the identity of the lost entry. [`Recovery`] lists what
//!   opening found and repaired: rebuilt records, a repaired metainfo copy,
//!   repaired segment headers, torn records.
//! - **Fault aiming.** [`Journal::layout`] and [`Journal::regions`] return the
//!   byte ranges of each record, entry, metainfo copy and segment header as
//!   [`LayoutRegion`](moonpool_core::LayoutRegion) values. A simulation uses them to aim
//!   storage faults at one structure. The position is the stripe key, so a
//!   replicated fault pattern hits the same position on every replica.
//! - **Spare segments.** [`Journal::prepare_spares_with`] prepares the next
//!   segment in the background on a [`TaskProvider`](moonpool_core::TaskProvider)
//!   once the active one is half full, so the commit that fills it rolls
//!   with a header write and a rename instead of writing a whole segment.
//!   [`Journal::prepare_spare`] does it inline; [`Journal::rolls`] counts
//!   the rolls of each kind.
//! - **Crash hints.** Each commit names three moments with
//!   [`hint!`](moonpool_buggify::hint!): entries written but not synced, records
//!   written but not synced, and the batch synced while the metainfo is
//!   still stale. In production these are no-ops. In a simulation the
//!   simulator may reboot the process there, under the seed's attrition
//!   regime and an optional harness `HintVeto` budget.
//!
//! No snapshots and no compaction: space comes back through the floor and
//! tombstones. CLSTORE's snapshot store is not part of this crate.
//!
//! # Feature flags
//!
//! This crate has no cargo features. It depends on `moonpool-core` with
//! `default-features = false`, so it pulls in no production provider. Add
//! `moonpool-core` with the `tokio-fs` feature to run it on a real
//! filesystem.
//!
//! # Files
//!
//! ```text
//! <dir>/meta                    metainfo copy A (block 0) and copy B (block 16)
//! <dir>/seg-<first batch>.wal   preallocated, zero-filled segments:
//!     header A | header B | persist log | guard gap | entry log
//! <dir>/spare-<gen>-<n>.wal     the next segment, prepared: zeros, no header
//! ```
//!
//! A **spare** is never evidence: it has no header, no batch is written to
//! it before it is renamed to a segment, and every open deletes the spares
//! it finds ([`Recovery::spares_removed`]).
//!
//! The **meta file** is separate because it is the one structure rewritten in
//! place, and it must outlive every segment (`LogCabin`, canonical/raft and
//! hashicorp/raft-wal keep it apart too). Each **segment** holds two
//! append-only logs:
//!
//! - **persist records** of [`RECORD_SIZE`] bytes: kind, position, id, and
//!   the entry's offset, length and CRC (CLSTORE's identifier and persist
//!   record in one);
//! - **entries**: a 64-byte header that repeats the record's identity, then
//!   the payload.
//!
//! Every batch starts on a fresh [`BLOCK`] in both logs, so no write touches
//! a sector that holds acknowledged data. A segment header links to the last
//! batch before it, so the journal never reads a missing segment as the end
//! of the log. [`Geometry::default`] is a 64 MiB segment;
//! [`Geometry::small`] rolls over quickly for tests and simulation.
//!
//! # Committing
//!
//! [`Durability`] picks the protocol:
//!
//! | Mode | Protocol | Syncs |
//! |---|---|---|
//! | [`Batched`](Durability::Batched) (the paper's) | `write(entries); write(records); fsync` | 1 |
//! | [`Ordered`](Durability::Ordered) | `write(entries); fsync; write(records); fsync` | 2 |
//!
//! The protocol is recorded per batch, so it may change between opens. When
//! a batch changes the metainfo (with [`Batch::set_meta`] or a floor raise),
//! the journal writes it only **after** the batch's last sync: copy A, sync,
//! copy B, sync. At most one copy is in flight at a time. A crash can leave a
//! batch durable without its metainfo, but never the metainfo without its
//! batch. The caller may therefore put in the metainfo anything that is true
//! only once the batch is on disk.
//!
//! A failed commit poisons the journal: [`CommitError::Io`] says through
//! [`Durable`] whether the batch may be on disk anyway, and the caller
//! reopens.
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
//! A batch missing while later ones exist, or a log that ends before the
//! batch the metainfo proves durable, is [`OpenError::LostBatch`]. A medium
//! error reads as damaged bytes (CLSTORE's treatment of EIO). A damaged or
//! missing file is a crash ([`OpenError::BadSegment`],
//! [`OpenError::MetaLost`]). Opening reads only the last batch's entries; the
//! journal checks the rest when they are read ([`Journal::read`],
//! [`Journal::replay`]). Opening syncs what it keeps before it returns.
//!
//! # Examples and tests
//!
//! - `examples/accept_log.rs`: a Paxos acceptor on the real filesystem finds
//!   one rotted accept, with its ballot, and repairs it.
//! - `examples/bench.rs`: commit throughput, latency percentiles and
//!   fairness on the real filesystem, per writer count, record size, batch
//!   size and [`Durability`].
//! - `tests/journal.rs`: one test per row of the recovery table, aimed with
//!   [`Journal::layout`], plus crash loops under two fault models.
//! - `moonpool-sim-examples/src/journal.rs`: the journal in a full
//!   simulation, one node crashed over and over by attrition.

mod batch;
mod error;
mod format;
mod io;
mod journal;
mod meta;
mod recover;
mod segment;
mod spare;

pub use batch::Batch;
pub use error::{CommitError, Durable, OpenError, ReadError};
pub use format::{BLOCK, Geometry, ID_SIZE, Id, MAX_BATCH_RECORDS, META_MAX, RECORD_SIZE};
pub use journal::{
    Durability, Entry, Journal, JournalConfig, JournalId, Layout, Recovery, Rolls, State,
};
