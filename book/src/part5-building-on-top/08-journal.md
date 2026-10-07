# A Crash-Aware Journal

<!-- toc -->

`moonpool-journal` is a write-ahead journal built on nothing but the provider
traits and [`BlockFile`](../part2-foundations/07-provider-traits.md). It
implements CLSTORE, the local storage layer of *Protocol-Aware Recovery for
Consensus-Based Storage* (Alagappan et al., FAST '18), and doubles as a worked
example of testing a storage engine against the simulator's disk.

## The Problem It Solves

A log that finds a bad checksum usually truncates there. For a torn write at
the tail that is correct: the entry was never acknowledged. For an entry that
was acknowledged and later rotted, it silently throws away committed data,
and in a replicated system a node that truncated can then win an election
and erase it cluster-wide. The two cases look identical if all the log has
is the entry.

CLSTORE does three things about it. It **detects** damage (checksums, and a
medium error read as damaged bytes). It **disentangles** a crash from
corruption: every entry has a *persist record* stored far from it, and a
damaged entry whose record survived was written, so it is corruption, never
a torn write. And it **identifies** what was damaged: the record carries the
entry's identity, so a replication layer knows exactly what to fetch from a
peer.

## Positions, Not Indexes

The journal knows no protocol. An entry is a `u64` **position** and an
opaque 24-byte [`Id`](https://docs.rs/moonpool-journal) the caller chooses:
a Paxos acceptor puts its slot in the position and its ballot in the id, a
Raft log its index and term. Positions are written in any order, with gaps,
and overwritten:

```rust,ignore
let mut batch = Batch::new();
batch
    .put(7, ballot(3), b"set x=7".to_vec())   // a new slot
    .put(4, ballot(2), b"set w=4".to_vec())   // a lagging acceptor catching up
    .put(9, ballot(5), b"set y=10".to_vec())  // a re-accept at a higher ballot
    .set_meta(b"promise=5".to_vec());         // the node-unique state
journal.commit(batch).await?;
```

That shape is what a Paxos acceptor or a lagging replica needs, and it is
why the journal stores state rather than a log of operations: there is no
fold at boot and no checkpoint. `clear(range)` removes positions (a Raft
suffix truncation), `truncate_prefix(floor)` drops everything below a floor,
and `set_meta` replaces up to 4 KiB of metainfo kept in two copies. There
are no snapshots and no compaction: space comes back through the floor and
tombstones.

A batch lands in one segment, so it must fit an empty one; a bigger one is
refused with `BatchTooLarge` before anything is written. A caller with more
to write than that (a replica catching up a long log at once) packs its
writes with `Batch::fits_another(geometry, payload_len)` and commits each
batch in turn, metainfo and floor in the last.

## Layout

```text
<dir>/meta                   metainfo copy A (block 0), copy B (block 16)
<dir>/seg-<first batch>.wal  preallocated, zero-filled:
    header A | header B | persist log | guard gap | entry log
```

Each segment holds two append-only logs. The **persist log** is a run of
64-byte records: kind, position, id, the entry's offset, length and CRC, and
the batch, its record count and the record's index in it. The **entry log**
holds the payloads in arrival order, each behind a 64-byte header that
repeats the record's identity, so a stale or misdirected entry never passes
for the one a record names, and an intact entry can rebuild a lost record
whole. A guard gap keeps the two physically apart.

Every batch starts on a fresh block in both logs, so no write ever touches a
sector holding anything acknowledged. A segment header links to the last
batch before the segment, so a missing segment file is refused rather than
read as the end of the log. Segments are deleted only from the front, once
they hold nothing live, because a later segment may hold the write or the
tombstone that made an earlier one dead.

The **meta file** is separate: it is the one structure rewritten in place,
and it must outlive every segment. `LogCabin`, canonical/raft and
hashicorp/raft-wal keep their term and vote apart from the log for the same
reasons. Its two copies are written one after the other, each followed by a
sync, so at most one is ever in flight and a single rotted copy never rolls
the metainfo back.

## Committing: One Sync or Two

`Durability` picks the protocol, and every batch records which one it used:

| Mode | Protocol | Syncs | Last batch |
|---|---|---|---|
| `Batched` | `write(entries)`, `write(records)`, `fsync` | 1 | ambiguous |
| `Ordered` | `write(entries)`, `fsync`, `write(records)`, `fsync` | 2 | decided |

`Batched` is the paper's protocol. Its Appendix A proves the limitation it
carries: with one sync, a crash can leave an intact record beside a damaged
entry exactly as corruption would, so the last batch's damaged entries are
reported `Ambiguous` and the replication layer decides. `Ordered` writes the
records only once the entries are durable, so a record always proves its
entry was synced, at the price of a second sync. When a batch changes the
metainfo, copy A goes in the first window and copy B in the second.

## Recovering

Opening reads every persist record, locates each batch from any one of its
records (or from its first entry when every record is damaged), walks the
entry log to match damaged records with their entries, and judges each one:

| Entry | Persist record | Verdict |
|---|---|---|
| intact | valid | live |
| intact | damaged | live; the record is rebuilt from the entry |
| damaged | valid | `Corrupt`, or `Ambiguous` in a `Batched` last batch |
| damaged | damaged, last batch | torn by a crash: discarded |
| damaged | damaged, earlier | `DoubleFault`: refuse to open |

A batch missing while later ones exist, or a log ending before the batch the
metainfo proves durable, is `LostBatch`. A missing meta file beside segments,
or two damaged copies, is `MetaLost`. Only the last batch's entries are read
at open. The rest are checked when read, and `replay` reports a damaged one
in place and goes on.

Three rules keep recovery's own writes honest, and the crash loops found
each of them:

- **What opening keeps, it syncs.** A process that restarted without a power
  loss reads its predecessor's unsynced writes from the page cache.
- **A record never reaches the disk before its entry.** When opening voids a
  torn record or rebuilds one, it rewrites the last batch's records, and the
  entries it rebuilt them from may still be in the page cache: it syncs them
  first, as a commit does.
- **What an open declared the end stays the end.** Every open durably bumps
  an *open generation* in the metainfo and records the last batch it found;
  batch numbers carry their generation in their high bits. A record a crash
  left unreadable (a latent sector) can read back later, after the log moved
  on: one from an older generation numbered beyond what the next open found
  is garbage, never evidence.

## Two Fault Models

The integration tests crash a writer over and over against a ledger of what
it acknowledged and what was in flight:

- **The paper's:** every unsynced sector ends up old or new, plus clean
  crashes and correlated rollbacks.
- **Moonpool's full physics:** sectors lost to garbage, latent read faults,
  shorn writes, on top. These destroy whatever an in-flight sector held.

The promise is the same under both: nothing acknowledged is lost or changed
or reported damaged, and only a `Batched` batch in flight may come back
ambiguous. Each runs for `Ordered`, `Batched`, and a mode drawn afresh every
round, with syncs failing, restarts that keep the page cache, and crashes
inside recovery itself. `JOURNAL_CRASH_SEEDS` raises the seed count.

## Aiming Faults at the Layout

A random fault rarely lands on the bytes that matter. The journal lists its
regions as `LayoutRegion`s, so a harness can aim faults by kind without
knowing the format:

```rust,ignore
let focus = FaultFocus::new()
    .background(0.1)
    .layout(&journal.regions(), |region| {
        if region.kind == Layout::RECORD { 8.0 } else { 4.0 }
    });
```

Records and entries carry their position as the region's `stripe`, the key
every replica's copy of that position shares, so a replicated harness can
damage a position on some replicas and never on all of them.
`journal.layout(position)` gives one position's two regions, which is how
the tests aim a single bit flip at a record or an entry.

## The Example Simulation

[`journal.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim-examples/src/journal.rs)
in `moonpool-sim-examples` (`cargo xtask sim run journal`) is the journal
inside a full simulation: one process owns it on its simulated disk,
`Chaos::Attrition` crashes that process over and over (never wiping it, since
a wiped disk is data loss no local journal can recover from), and
`Chaos::Storage(Random)` runs the disk's fault families underneath, minus the
ones a lone node has no second copy to repair from (rot, EIO, misdirected and
phantom writes, a failed disk).

The node's memory dies with it, so what it acknowledged lives in a `Ledger`
published in the iteration's `StateHandle`. Every boot draws a durability
mode, opens the journal, and checks it against that ledger before writing
again:

```rust,ignore
assert_always!(written, "a position holds only a value written there");
assert_always!(
    pending.batched && pending.touches(position),
    "only the Batched batch in flight comes back damaged"
);
assert_always!(present, "no acknowledged write is lost");
```

Then it commits batches at random positions, with tombstones, floor raises
and metainfo. A batch joins the ledger only once `commit` returns.

## On a Real Disk

The journal is written against the provider traits only, so production hands
it `TokioStorageProvider` instead of the simulator's storage.
[`accept_log.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-journal/examples/accept_log.rs)
(`cargo run -p moonpool-journal --example accept_log`) does exactly that, in a
temporary directory: a Paxos acceptor stores its accepts at their slots in
arrival order with the ballot as their identity and its promise as
metainfo, restarts and replays. Then it flips one byte of an acknowledged
accept on disk, behind the journal's back: reading it reports the slot
damaged with the ballot its record kept, the acceptor writes a peer's copy
at the same slot, and a final reopen comes back clean.
