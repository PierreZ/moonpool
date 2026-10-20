# A Crash-Aware Journal

<!-- toc -->

`moonpool-journal` is a write-ahead journal built on nothing but the provider
traits and [`BlockFile`](../part2-foundations/07-provider-traits.md). It
implements CLSTORE, the local storage layer of *Protocol-Aware Recovery for
Consensus-Based Storage* (Alagappan et al., FAST '18). It is a real crate:
production hands it `TokioStorageProvider`, tests hand it the simulator's
disk, and the code is the same. It is also the worked example of testing a
storage engine under `Chaos::Storage`.

## The Problem It Solves

A log that finds a bad checksum usually truncates there. For a torn write at
the tail that is correct: the entry was never acknowledged. For an entry
that was acknowledged and later rotted, truncation silently throws away
committed data. In a replicated system, the node that truncated can then win
an election and erase that data cluster-wide. The two cases look identical
if all the log has is the entry.

CLSTORE does three things about it. It **detects** damage (checksums, and a
medium error read as damaged bytes). It **disentangles** a crash from
corruption: every entry has a persist record stored far from it, and a
damaged entry whose record survived was written, so it is corruption, not a
torn write. And it **identifies** what was damaged: the record carries the
entry's identity, so a replication layer knows what to fetch from a peer.

## Positions, Not Indexes

The journal knows no protocol. An entry is a `u64` **position** and an
opaque 24-byte `Id` the caller chooses. A Paxos acceptor puts its slot in
the position and its ballot in the id. A Raft log puts its index and term.
Positions are written in any order, with gaps, and overwritten:

```rust,ignore
use moonpool_journal::{Batch, Journal, JournalConfig, JournalId};

let mut journal = Journal::create(provider, "wal", JournalId(0xACCE), JournalConfig::default(), b"promise=0").await?;
let mut batch = Batch::new();
batch
    .put(7, ballot_id(3), "set x=7")    // a new slot
    .put(4, ballot_id(2), "set w=4")    // a lagging acceptor catching up
    .put(9, ballot_id(5), "set y=10")   // a re-accept at a higher ballot
    .set_meta("promise=5");             // the node-unique state
journal.commit(batch).await?;
```

That shape is what an acceptor or a lagging replica needs, and it is why the
journal stores state rather than a log of operations: there is no fold at
boot and no checkpoint. `Batch::clear(range)` removes positions (a Raft
suffix truncation). `Batch::truncate_prefix(floor)` drops everything below
a floor and frees whole segments from the front. `Batch::set_meta` replaces
the metainfo (at most `META_MAX` bytes, just under 4 KiB), kept in two
copies. There are no snapshots and no compaction.

A batch lands in one segment, so it must fit an empty one. A bigger batch is
refused with `CommitError::BatchTooLarge` before anything is written. A
caller with more to write than that (a replica catching up a long log) packs
its writes with `Batch::fits_another`:

```rust,ignore
let mut chunk = Batch::new();
for (position, payload) in puts {
    if !chunk.fits_another(geometry, payload.len()) {
        journal.commit(std::mem::take(&mut chunk)).await?;  // every chunk fits
    }
    chunk.put(position, id, payload);
}
journal.commit(chunk).await?;   // metainfo and floor go in the last one
```

`Batch::fits(geometry)` is the same bound for a staged batch. The default
`Geometry` is a 64 MiB segment with persist and entry logs of the same size
(507,904 records and 31 MiB of entries). Every batch starts on a fresh block
in both logs, so a segment holds 7,936 batches of one small entry.
`Geometry::small()` is 512 records and 64 KiB of entries, so tests roll over
segments quickly. `Journal::open` refuses a geometry other than the one the
journal was created with.

## Layout

```text
<dir>/meta                   metainfo copy A (block 0), copy B (block 16)
<dir>/seg-<first batch>.wal  allocated whole:
    header A | header B | persist log | guard gap | entry log
<dir>/spare-<gen>-<n>.wal    a dead segment's file, kept for the next roll
```

Each segment holds two append-only logs. The **persist log** is a run of
64-byte records: kind, position, id, the entry's offset, length and CRC, the
batch, its record count and the record's index. The **entry log** holds the
payloads in arrival order, each behind a 64-byte header that repeats the
record's identity, so a misdirected entry never passes for the one a record
names, and an intact entry can rebuild a lost record. Every batch starts on
a fresh block in both logs, so **no write ever touches a sector holding
anything acknowledged**. That one rule is what makes the crash model below
survivable.

The meta file is separate. It is the one structure rewritten in place, and
it must outlive every segment. Its two copies are written one after the
other, each a durable write (`write_at_durable`), so at most one is in flight and a single
rotted copy never rolls the metainfo back. `Journal::peek_meta(&provider,
dir, id)` reads it without opening the journal: no recovery, no repair, no
sync, so a probe never changes what the next open finds.

## Committing: One Sync or Two

`Durability` picks the protocol, and every batch records which one it used:

| Mode | Protocol | Syncs | Last batch |
|---|---|---|---|
| `Batched` | `write(entries)`, `write(records)`, `fsync` | 1 | ambiguous |
| `Ordered` (default) | `write_durable(entries)`, `write_durable(records)` | 2 | decided |

`Batched` is the paper's protocol. With one sync, a crash can leave an intact
record beside a damaged entry exactly as corruption would. The paper's
Appendix A proves the two cannot be told apart, so the last batch's damaged
entries are reported `State::Ambiguous` and the replication layer decides.
The **ambiguous tail** stays ambiguous until the next batch commits, then
becomes `Corrupt`. `Ordered` writes the records only once the entries are
durable, so a record always proves its entry was synced.

`Ordered` issues each log's write as one durable write
(`StorageFile::write_at_durable`): on Linux one `pwritev2(RWF_DSYNC)`, which
on a direct-I/O segment whose blocks are written can become a single FUA
write instead of a write plus a cache flush. Two calls replace four, and the
durability argument is unchanged. A durable write covers its own range only,
and that range is the segment's one unsynced write: every earlier commit was
durable before it returned, an open syncs every segment, a roll writes its
headers durably, and a failed commit poisons the journal. Data-only
durability is enough because a segment never changes size. `Batched` keeps
its plain writes and one sync: a durable write of the records alone would not
cover the entries.

### No Copy on the Way to the Disk

A commit encodes its batch **in place**. `commit` sizes the entry log's
bytes first (`Batch::fits` already bounded them), allocates one zeroed,
block-aligned `AlignedBuf`, and encodes every entry header and payload
straight into it; the persist records go into a second one. Both move into
`BlockFile::write_blocks_aligned` / `write_blocks_durable_aligned`, which
hand them to the file's owned `write_at_aligned` /
`write_at_durable_aligned`. On `TokioStorageProvider` the buffer moves into
the blocking task and back, so `pwritev2(RWF_DSYNC)` reads the journal's own
memory: the bytes a caller staged are copied once, into the batch, and
never again. The segment headers, the metainfo copies and recovery's
reads (the persist log, entries read in place) take the same owned path.
Before, a direct-I/O commit copied each log twice: from the journal's
`Vec` into an aligned buffer, then into the provider's bounce buffer.

```rust,ignore
let entries = std::mem::take(&mut batch.entries);
batch.entries = segment.write_durable(batch.entry_at, entries).await?; // moved, not copied
```

A buffer whose address missed a file's memory alignment would be copied
once into one that meets it; a block boundary meets every direct-I/O device
the journal opens in practice.

Metainfo is written only **after** the batch is durable: copy A durably,
then copy B durably. A crash can leave the batch durable without its metainfo, but never
the metainfo without its batch. Anything true only once a batch is on disk
(a chosen index, a watermark) can share that batch's commit.

### Hints Inside a Commit

The commit protocol names its dangerous moments with `hint!` from
`moonpool-buggify`. A batched commit names three:

```rust,ignore
batch.entries = segment.write(batch.entry_at, entries).await?;
// A power loss here tears entries no record points at yet.
moonpool_buggify::hint!("journal commit: entries written, not synced", COMMIT_HINT_PROB).await;
batch.records = segment.write(batch.persist_at, records).await?;
moonpool_buggify::hint!("journal commit: records written, not synced", COMMIT_HINT_PROB).await;
segment.sync().await?;
if let Some(meta) = meta {
    moonpool_buggify::hint!("journal commit: batch synced, metainfo stale", COMMIT_HINT_PROB).await;
    meta_file.update(meta).await?;
}
```

An ordered commit writes each log with one durable write, so the window
between a write and its sync lies inside a single call. The simulator names
it there: its `write_at_durable` (which the owned `write_at_durable_aligned`
calls by default) is `write_at`, then
`hint!("storage: durable write landed, not synced")` at the same 0.25, then
`sync_data`. The ordered commit keeps the metainfo moment of its own.

In production a hint is a no-op. In a simulation the `HintSink` decides
whether to crash the owning process at that exact moment, under the seed's
attrition regime and swarm mask (see
[hint!: Naming Fault Sites](../part3-building/29-hints.md)). The rate is
0.25 per call, higher than the default `POINT_PROB`, because a commit is
short and a cut inside it is the case recovery exists for. A harness with a
budget the simulator cannot see, such as the copies a replicated store can
lose before a crash is no longer survivable, publishes a `HintVeto`:

```rust,ignore
HintVeto::new(move |ip, label| copies_alive.load(Ordering::Relaxed) > quorum)
    .publish(ctx.state());
```

The sink asks the veto last, after the regime admits the reboot, so a `true`
answer is a reboot.

### Rolling Onto a Recycled Segment

When the active segment is full, the commit rolls onto a new one. How
expensive should that be? A segment is 64 MiB. Write it with zeros and the
commit waits for 64 MiB and two syncs; write it in a background task and the
zeros still compete with every commit for the same disk. We measured a
zero-fill running beside the commits: they ran five times slower, with
45 ms spikes.

So the journal writes no zeros at all. It does what PostgreSQL does with its
WAL: it **recycles**. When the floor, or later writes, leave the front
segment with nothing live, the journal renames that file to
`spare-<generation>-<n>.wal` and syncs the directory, instead of deleting
it. The next roll **adopts** the spare: it writes the two header blocks,
syncs them, renames the file to `seg-<first batch>.wal` and syncs the
directory. One small durable write and a rename. The file is already full
size, and every block in it was written before, so a commit into it costs
exactly what a commit into any written block costs.

With no spare (a journal that only grows), the roll creates the segment with
`StorageFile::allocate`, which is `fallocate(2)` on Linux: the space is
reserved, the range reads as zeros, and only the headers are written. A full
disk fails there, at the roll, and never inside a commit. A new segment's
first pass does pay a little: the first durable write into each block also
records that the block is now written, a small metadata change. A recycled
segment pays none.

But a recycled file is full of old batches. Why are they not evidence?

- **A batch older than its segment is stale.** A segment named after its
  first batch only ever receives batches at or above that number, and batch
  numbers never repeat: the open generation lives in their high bits. Every
  record and entry a recycled file still holds is older than the segment it
  becomes, so the scan reads it like a never-written slot. The recovery
  table below is unchanged.
- **No batch is written to a spare.** Its headers are synced before its
  rename, so a crash during the adoption leaves the spare, or a whole empty
  segment, or the same file under either name. A recycled segment is renamed
  and its directory synced before anything is written into it, so it can
  never come back under its old name with new bytes in it.
- **The commit acknowledges its batch only after the directory sync**, so
  the segment's name is durable before any batch in it is.

An open keeps one whole spare it finds (`Recovery::spare_kept`) and deletes
the others (`Recovery::spares_removed`). `Journal::rolls()` counts the rolls
of each kind. The commit side names two moments with `hint!` ("journal roll:
spare headers synced, not renamed", "journal roll: spare renamed, directory
not synced").

## Recovering

Opening reads every persist record, locates each batch, walks the entry log
to match damaged records with their entries, and judges each one:

| Entry | Persist record | Verdict |
|---|---|---|
| intact | valid | live |
| intact | damaged | live; the record is rebuilt from the entry |
| damaged | valid | `Corrupt`, or `Ambiguous` in a `Batched` last batch |
| damaged | damaged, last batch | torn by a crash: discarded |
| damaged | damaged, earlier | `OpenError::DoubleFault`: refuse to open |

A batch missing while later ones exist, or a log ending before the batch the
metainfo proves durable, is `LostBatch`. A missing meta file beside segments,
or two damaged copies, is `MetaLost`. Opening returns a `Recovery` that says
what it found and did: `corrupt`, `ambiguous`, `torn`, `rebuilt`,
`meta_repaired` (a metainfo copy rewritten from its twin),
`headers_repaired`, `segments_removed`, `spares_removed` and `spare_kept`. Only the last batch's entries are
read at open. The rest are checked when read: `read(position)` returns
`ReadError::Damaged { position, id }` with the identity its record kept, and
`replay(range)` reports a damaged entry in place and goes on. A replay reads
entries that follow each other in one segment with one read, up to 1 MiB, and
judges each entry as `read` does: under direct I/O, a replay of a few thousand
entries costs a few reads, not one read per entry.

Three rules keep recovery's own writes honest, and the crash loops found
each of them. **What opening keeps, it syncs**, because a process that
restarted without a power loss reads its predecessor's unsynced writes from
the page cache. **A record never reaches the disk before its entry**, so a
rebuilt record is written only after the entries are synced. **What an open
declared the end stays the end**: every open bumps an open generation in the
metainfo, and a record a crash left unreadable can never read back later as
evidence.

## Testing It Under Chaos::Storage

The integration tests in `tests/journal.rs` run on `SimWorld` storage with
`StorageConfiguration::fast_local()`. One test per row of the recovery table
aims a bit flip with `Journal::layout(position)`, which returns the record
and the entry as two `LayoutRegion`s, each carrying the position as its
`stripe`. `Journal::regions()` lists every region a fault could hit, so a
harness aims faults by kind without knowing the format:

```rust,ignore
let focus = FaultFocus::new()
    .background(0.1)
    .layout(&journal.regions(), |region| {
        if region.kind == Layout::RECORD { 8.0 } else { 4.0 }
    });
```

The crash loops in `tests/support/mod.rs` commit random batches, cut the
writer off mid-commit with `simulate_crash_for_process`, sometimes crash
inside recovery itself, and judge every reopen against a ledger of what was
acknowledged and what was in flight. They run under two fault models:

- **The paper's:** every unsynced sector ends up old or new, plus clean
  crashes and correlated rollbacks (`clean_crash_probability`,
  `correlated_rollback_probability`, `garbage_fill_probability`).
- **Moonpool's full physics:** sectors lost, latent read faults and shorn
  writes on top (`crash_lost_probability`, `crash_latent_fault_probability`,
  `shorn_write_probability`), with failed syncs and lost directory entries.

The promise is the same under both: nothing acknowledged is lost, changed or
reported damaged, and only a `Batched` batch in flight may come back
ambiguous. Each model runs for `Ordered`, `Batched` and a mode drawn afresh
every round. Floor raises free segments, so crashes land around recycling
and commits roll onto recycled segments; each model asserts that commits
rolled onto a recycled segment and that an open kept a spare. `JOURNAL_CRASH_SEEDS` raises the seed count and
`JOURNAL_CRASH_SEED` replays one.

The full simulation is `journal.rs` in `moonpool-sim-examples`
(`cargo xtask sim run journal`). One process owns the journal.
`Chaos::Attrition` crashes it over and over with `prob_wipe: 0.0`, since a
wiped disk is data loss no local journal can recover from.
`Chaos::Storage(ChaosMode::Random)` runs the disk's fault families
underneath, with a `storage_fault_mask` that removes the ones a lone node has
no second copy to repair from:

```rust,ignore
.storage_fault_mask(
    StorageFaultMask::all()
        .without(StorageFault::Corruption)
        .without(StorageFault::Eio)
        .without(StorageFault::Misdirect)
        .without(StorageFault::PhantomWrite)
        .without(StorageFault::DiskFailure),
)
```

Floor raises recycle segments under the crashes, so the run reaches "an
open found a recycled spare segment" and "a commit rolled onto a recycled
segment".

The node's memory dies with it, so what it acknowledged lives in a `Ledger`
published in the iteration's `StateHandle`. Every boot draws a durability
mode, opens the journal and checks it against that ledger:

```rust,ignore
assert_always!(written, "a position holds only a value written there");
assert_always!(
    pending.batched && pending.touches(position),
    "only the Batched batch in flight comes back damaged"
);
assert_always!(present, "no acknowledged write is lost");
```

A failed sync is an operating error, not a verdict: `CommitError::Io` says
whether the batch may be durable anyway (`Durable::Unknown`), the journal is
poisoned, and the node reopens it.

## On a Real Disk

`accept_log.rs` (`cargo run -p moonpool-journal --example accept_log`) runs
the same journal on the real filesystem through `TokioStorageProvider`, in a
temporary directory. A Paxos acceptor stores accepts at their slots in
arrival order with the ballot as identity and its promise as metainfo, then
restarts and replays. Then it flips one byte of an acknowledged accept
behind the journal's back. Reading it reports the slot damaged with the
ballot its record kept, the acceptor writes a peer's copy at the same slot,
and a final reopen comes back clean. Nothing in the crate names the
simulator: the provider is the only thing that changes.

Does the provider change the speed? `backends.rs` answers it. It runs the
same journal on Tokio and on compio, with the same payloads, buffered and
with direct I/O (`cargo run --release -p moonpool-journal --features
compio-providers --example backends`). On a 4-vCPU cloud VM, compio commits
one writer's small records about **15 to 20% faster**. Each Tokio file
operation goes through the blocking pool, which costs about 17 syscalls per
append against 3.5 on `io_uring`. With 16 journals in flight, Tokio commits
**25 to 72% more**, because compio runs all 16 on one thread. Recovery
takes the same time on both. The tables, the environment and the caveats
are in
[`docs/benchmarks/journal-backends.md`](https://github.com/PierreZ/moonpool/blob/main/docs/benchmarks/journal-backends.md).
The VM very likely hides most of the cost of a sync, so measure on your own
disk.
