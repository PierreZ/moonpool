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
//! The journal uses only moonpool's local provider traits
//! ([`LocalStorageProvider`](moonpool_core::LocalStorageProvider)), which
//! every `Send` provider also implements. The same code runs over
//! `TokioStorageProvider` in production, over `CompioStorageProvider` on a
//! thread-per-core runtime, and over the simulator's `SimStorageProvider` in
//! tests. Over a `Send` provider the journal and its futures are `Send`. It knows no protocol: an entry is a `u64`
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
//!   the caller whether a batch fits one segment of a [`Geometry`] (each write
//!   counted with room for one retire record), so it can
//!   split its writes across commits before [`Journal::commit`] refuses them.
//! - **Two commit protocols.** [`Durability::Batched`] (one sync) and
//!   [`Durability::Ordered`] (two syncs), chosen per open.
//! - **Damage is evidence.** [`State::Corrupt`], [`State::Ambiguous`],
//!   [`ReadError::Damaged`] and the evidence variants of [`OpenError`] report
//!   damage with the identity of the lost entry. [`Recovery`] lists what
//!   opening found and repaired: rebuilt records, a repaired metainfo copy,
//!   repaired segment headers, torn records.
//! - **A scrub of the persist records.** [`Journal::verify_records`] reads
//!   the records of a range from disk and names the positions whose record
//!   rotted since the open. [`Journal::repair_records`] writes back every
//!   record that is damaged while its entry is intact, the records of
//!   tombstones and replaced positions too, before a second fault on the
//!   same slot makes the next open refuse. Each open does the same for the
//!   batches before the last one ([`RecordRepair`]).
//! - **A scrub of everything.** [`Journal::scrub`] checks the journal against
//!   the device in steps, a byte budget per call: each pass rewrites any
//!   metainfo copy or segment header copy that differs from the value the
//!   open holds, checks each live entry without copying it, and repairs each
//!   rotted persist record from its entry. A damaged entry is named
//!   ([`Scrub::damaged`]) for the caller to fetch from a peer. One fault is
//!   then found while its twin still holds, before a second makes the next
//!   open refuse.
//! - **Fault aiming.** [`Journal::layout`] and [`Journal::regions`] return the
//!   byte ranges of each record, entry, metainfo copy and segment header as
//!   [`LayoutRegion`](moonpool_core::LayoutRegion) values. A simulation uses them to aim
//!   storage faults at one structure. The position is the stripe key, so a
//!   replicated fault pattern hits the same position on every replica.
//! - **Recycled segments, no zero-fill.** A dead segment's file becomes the
//!   spare, and the next roll adopts it with a header write and a rename.
//!   Its old batches are older than the segment it becomes, so they read
//!   as stale bytes, never as evidence. The same holds for old records
//!   beyond the end of a segment's log: batch numbers grow with the place
//!   in the persist log, so a record at or after a batch boundary whose
//!   batch is not above the last batch found before it is old bytes, at
//!   every open. With no spare, a roll allocates a
//!   new file ([`allocate`](moonpool_core::StorageFile::allocate)) and
//!   writes only its headers. No segment is ever zero-filled.
//!   [`Journal::prepare_spares`] allocates spares ahead of the rolls, up to
//!   [`JournalConfig::spares`], so no commit pays for an allocation.
//!   [`Journal::rolls`] counts the rolls of each kind.
//! - **Durable writes.** An ordered commit writes each log with one
//!   [`write_at_durable`](moonpool_core::StorageFile::write_at_durable)
//!   (`pwritev2(RWF_DSYNC)` on Linux) where a write and a sync were two
//!   calls; so do the metainfo copies and a roll's headers.
//! - **No copy on the way to the disk.** A commit encodes its entries and
//!   records in place, in block-aligned
//!   [`AlignedBuf`](moonpool_core::AlignedBuf)s, and moves them through the
//!   zero-copy [`BlockFile`](moonpool_core::BlockFile) writes
//!   ([`StorageFile::write_at_durable_aligned`](moonpool_core::StorageFile::write_at_durable_aligned)
//!   and its siblings): over `TokioStorageProvider` the kernel reads the
//!   journal's own buffer, and a direct-I/O segment never copies it.
//! - **Crash hints.** A batched commit names three moments with
//!   [`hint!`](moonpool_buggify::hint!): entries written but not synced, records
//!   written but not synced, and the batch synced while the metainfo is
//!   still stale. An ordered commit names the last one; the windows inside
//!   its two durable writes are the storage layer's to name. A commit that
//!   makes a damaged slot dead names one more: the batch durable, the
//!   retire batch for that slot not written yet. In production these are no-ops. In a simulation the
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
//! `moonpool-core` with the `tokio-providers` feature to run it on a real
//! filesystem.
//!
//! # Files
//!
//! ```text
//! <dir>/meta                    metainfo copy A (block 0) and copy B (block 16)
//! <dir>/seg-<first batch>.wal   allocated segments:
//!     header A | persist log | guard gap | entry log | header B
//! <dir>/spare-<gen>-<n>.wal     a dead segment's file, or one allocated ahead,
//!                               kept for a later roll
//! ```
//!
//! A **spare** is never evidence: no batch is written to it before it is
//! renamed to a segment, and the batches it still holds are older than any
//! it will hold. An open keeps up to [`JournalConfig::spares`] whole spares
//! and deletes the others ([`Recovery::spares_removed`]), and never keeps a spare whose header names
//! a listed segment: a crash in a roll left it as a second name of that
//! segment.
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
//!   the payload. The header has its own CRC, and holds the payload's CRC
//!   and a **stamp**, the metainfo `seq` it was written under.
//!
//! Every batch starts on a fresh [`BLOCK`] in both logs, so a commit never
//! touches a sector that holds acknowledged data. The one exception is a
//! record repair ([`RecordRepair`]): it writes a persist block again only
//! when the entry of each record in it is intact, so a crash that tears the
//! write leaves each record rebuildable. A segment header links to the last
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
//! reopens. A commit future dropped before it returns (a timeout, a losing
//! `select!` branch) is a failed commit too: the journal stays poisoned from
//! the commit's first await until memory follows the disk, so a dropped
//! commit never lets the next one write the same batch number over a batch
//! that is partly on disk.
//!
//! # Recovery
//!
//! Opening reads the persist records and judges each one (`last` is the
//! last batch in the log). The last segment is read whole. A sealed segment
//! ends at the batch that the next segment's header names: opening reads its
//! persist log only up to that batch, and a record past it is not part of
//! the segment.
//!
//! | Entry | Persist record | Verdict |
//! |---|---|---|
//! | intact | valid | live |
//! | intact | damaged | live; the record is rebuilt from the entry, and written back before `last` |
//! | damaged | valid | [`State::Corrupt`], or [`State::Ambiguous`] in a `Batched` last batch, or from the batch the metainfo names as ambiguous on |
//! | damaged | damaged, named by a retire record | ignored |
//! | damaged | damaged, in an unwitnessed last batch of its open generation | torn by a crash: discarded and retired |
//! | payload damaged, header intact | damaged, in a witnessed batch, or before the last of its generation | [`State::Corrupt`]: the header names the entry; its record is never written back; replaced by a later batch, it is retired, not reported |
//! | header damaged | damaged, in a witnessed batch, or before the last of its generation | [`OpenError::DoubleFault`]: refuse to open |
//!
//! A **retire record** names a slot of an earlier batch that holds nothing
//! the caller needs: a slot a crash tore, or the dead slot of a position
//! that a later batch replaced, cleared or dropped while its entry was known
//! damaged. Like a tombstone, it has a persist record and an entry, so one
//! fault on it is rebuilt. It carries the offset where the entry walk goes
//! on after the slot, so an intact entry behind a retired slot is found
//! again. An open that finds torn slots starts its new generation without
//! raising the high-water mark, commits one retire batch for them, and the
//! mark records that batch once it is durable: a crash anywhere in that
//! open leaves the slots judged the same way at the next one. A commit that
//! makes dead a slot whose entry is damaged ([`State::Corrupt`], or a read
//! that returned [`ReadError::Damaged`]) retires it in a batch of its own,
//! written once the commit's batch is durable. Never in the same batch: a
//! crash can tear that batch's put and keep its retire record, and the open
//! would discard the put as torn and ignore the old slot, the only copy of
//! an acknowledged entry.
//!
//! A batch is **witnessed** when something other than its own two damaged
//! parts proves it durable: the metainfo's high-water mark records it (a
//! commit that changed the metainfo records its own batch, and every open
//! records the last batch it found once nothing in it is left to retire), or
//! it is an [`Ordered`](Durability::Ordered)
//! batch with an intact record, whose entries were durable before any
//! record was written. A crash in the middle of a commit cannot leave a
//! witnessed batch with a slot whose record and entry are both damaged, so
//! there that is corruption, not a crash. In a witnessed batch, an entry
//! that is damaged while its record is valid is [`State::Corrupt`] too,
//! except one that an open already found damaged that way while the batch
//! was not witnessed: the metainfo names the first such batch, and its
//! entries stay [`State::Ambiguous`] until the caller commits a batch with a
//! write. An open's own mark raise and retire batch witness the batch's
//! intact slots, not the entry the crash had already torn.
//! Only the last batch of an open generation can be torn by a crash, because
//! a failed commit poisons the journal. The one batch that may still be
//! discarded is an acknowledged last batch that nothing witnesses: its bytes
//! look exactly like a torn commit.
//!
//! An entry header has a CRC of its own, apart from its payload's, so an
//! intact header with a damaged payload still names its entry and its
//! length. Three rules keep that from hiding a crash. In an unwitnessed last
//! batch it is still torn: a commit writes the header and the payload in one
//! write, and a crash can keep one without the other. A record is never
//! rebuilt from a header alone, because the header proves nothing of the
//! payload. And an `Ordered` batch is witnessed only by an intact record,
//! never by a header. A batch whose records are all damaged is located from
//! its first entry's header, payload or not.
//!
//! A record that the scan finds at a batch boundary but that names an entry
//! other than the first entry of that boundary was written to the wrong
//! block (a misdirected write): the open reads it as damaged and locates
//! the batch from its first entry.
//!
//! A batch missing while later ones exist, or a log that ends before the
//! batch the metainfo proves durable, is [`OpenError::LostBatch`]. Batch
//! numbers have no gaps (an open generation counts on from its first
//! number), so a missing batch is found even when the walk of the entries
//! goes straight past it: an open's retire batch that the disk lost, say,
//! whose entry sat over the slot it discarded. A batch of
//! a later open generation than the metainfo's proves the metainfo older
//! than the log (both copies lost an update): [`OpenError::MetaRolledBack`].
//! Inside one open generation, the stamps tell: an intact entry header
//! stamped with a later metainfo `seq` than the one that reads back proves
//! that the metainfo lost an update ([`OpenError::MetaUpdateLost`]). A batch
//! is stamped with the metainfo that is durable when it is written, so the
//! batch that carries a metainfo update holds the stamp before it, and a
//! crash before the update leaves nothing ahead. The open checks the stamps
//! of the entries it reads: the last batch, and the entries that rebuild a
//! record or locate a batch. A batch
//! number holds the open generation in its high 24 bits and the count in
//! its low 40 bits: an open after generation 2^24 - 1 refuses
//! ([`OpenError::InvalidConfig`]), and a commit after 2^40 - 1 batches in
//! one open refuses ([`CommitError::BatchNumbersExhausted`]) before it
//! writes; a reopen starts a new generation. A medium
//! error reads as damaged bytes (CLSTORE's treatment of EIO). A damaged or
//! missing file is a crash ([`OpenError::BadSegment`],
//! [`OpenError::MetaLost`]). An intact metainfo or segment header written
//! by another version of the crate is not damage: it is
//! [`OpenError::UnsupportedVersion`], because each checksum covers the
//! version field. Opening reads only the last batch's entries; the
//! journal checks the rest when they are read ([`Journal::read`],
//! [`Journal::replay`]). Opening syncs what it keeps before it returns,
//! and reads only what the device holds: it syncs each file, then drops its
//! cached pages, before it reads it (on Linux: see
//! [`StorageFile::drop_cache`](moonpool_core::StorageFile::drop_cache)). A
//! commit that failed because the
//! kernel could not write its pages back (fsyncgate) leaves them marked
//! clean in the page cache, where a restart without a power loss would
//! otherwise read them as written.
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
    Durability, Entry, Journal, JournalConfig, JournalId, Layout, RecordRepair, Recovery, Rolls,
    Scrub, State,
};
