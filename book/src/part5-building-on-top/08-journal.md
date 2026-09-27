# A Crash-Aware Journal

<!-- toc -->

`moonpool-journal` is a write-ahead log built on nothing but the provider
traits and [`BlockFile`](../part2-foundations/07-provider-traits.md). It
implements CLSTORE, the local storage layer of *Protocol-Aware Recovery for
Consensus-Based Storage* (Alagappan et al., FAST '18), and doubles as a worked
example of testing a storage engine against the simulator's disk.

## The Problem It Solves

A log that finds a bad checksum usually truncates there. For a torn write at
the tail that is correct: the entry was never acknowledged. For an entry that
was acknowledged and later rotted, it silently throws away committed data —
and everything after it. The two look identical if all the log has is the
entry.

CLSTORE stores each entry's identifier (index, epoch, tag, offset, length,
CRC) in a **slot**, in a table at the front of the segment, megabytes away
from the entry. A failed entry with an intact slot was written and
acknowledged; a failed entry without one was not.

## Identity Wider Than a Term

Raft names an entry by its index and term, and the slot records both. Not
every consensus log fits that shape: a Paxos acceptor accepts slots out of
order and re-accepts a slot at a higher ballot, so it journals its accepts as
a write-ahead log of operations — appended under a sequence number, replayed
latest-wins — and the identity a peer needs to repair a lost record is the
Paxos slot and ballot, not the sequence number. Each `Record` therefore
carries a 24-byte opaque `tag`, stored in the entry header *and* in the slot:

```rust,ignore
let tag = paxos_tag(slot, ballot); // the caller's encoding
journal
    .append(&[Record::new(ballot.round, &bytes).with_tag(tag)])
    .await?;
```

When the entry's bytes are lost, `Recovery::corrupt` and
`JournalError::Corrupt` carry an `EntryId { index, epoch, tag }` read from
the slot, so the caller knows exactly which slot and ballot to fetch again.
The tag is covered by both CRCs and repeated in both places, so an intact
entry still rebuilds a lost slot whole.

## Layout

```text
Offset   Region        Contents
0 KiB    Header A      magic, version, first_index,
4 KiB    Header B      slot_count, data_start, crc
8 KiB    Slot table    32,768 slots × 64 B (2 MiB)
~2 MiB   Guard gap     zeros (keeps IDs ≥2 MiB away)
4 MiB    Data region   append-only entries → end
```

Each segment is one preallocated, zero-filled file named after its first
index; the journal finds them with `StorageProvider::list_dir`. Real zeros
mean `fdatasync` never has metadata to write, "empty" reliably means all
zeros, and a file of the wrong size is itself a detectable fault. Log indexes
are dense and slots fixed-size, so slot *i* sits at a computed offset and the
slot table is a plain array.

## Appending

Per batch: write the entries contiguously into the data region, write their
slots in one call, `fdatasync` once, acknowledge. Nothing orders the first
write before the second — one sync per batch instead of two, with recovery
doing the disambiguation.

## Recovering

Opening walks every index. Entry *i*'s offset comes from its slot, or — when
the slot is unusable — from the end of entry *i − 1*. Then:

| Entry | Slot      | Action                                     |
|-------|-----------|--------------------------------------------|
| good  | valid     | keep                                       |
| good  | empty/bad | keep, rewrite slot                         |
| bad   | valid     | mark corrupt, report its identity upward   |
| bad   | empty     | torn tail: truncate here                   |
| bad   | bad       | double fault: refuse to start              |

The first entry without an identifier ends the log; every earlier faulty
entry that has one is corruption. The one exception is the paper's theorem:
the *last* entry, slot present and entry bad, is exactly what a crash between
the slot write and the sync leaves, so nothing local can tell it from
corruption. It is always reported as `ambiguous_tail`; what happens next is
the caller's choice, `JournalConfig::ambiguous_tail`:

- `AmbiguousTail::Truncate` (the default) treats it as a torn write — the
  only safe move for a single node, whose alternative is refusing to start.
- `AmbiguousTail::Keep` leaves it in the log, marked corrupt, for a
  replication layer to keep if committed and discard (with
  `truncate_suffix`) if not. Truncating would erase the only local evidence
  that the entry existed: one more crash before the caller acted on the
  report, and a Paxos acceptor could answer "nothing accepted here" for a
  vote it may have cast — the CTRL paper's Figure 2 bug.

An EIO is zero-filled into a checksum mismatch, as in the paper, so an
unreadable entry is reported corrupt rather than stopping the journal. And
recovery cleans up after itself like any truncation: the discarded slots and
entry bytes are zeroed and synced before the first append.

## Replaying

`Journal::read_range(start..next)` is the replay a caller runs after
opening. Entries are packed back to back, so each segment's share of the
range comes back in large sequential reads instead of one read per entry,
with exactly `read`'s checks; a corrupt entry is returned in place as
`Err(EntryId)` and the replay goes on.

## Metadata

Term/vote-style metadata (`save_meta`, `meta`) lives beside the segments in
two copies, `meta.0` and `meta.1`, each with a generation and a CRC, each
replaced through a temporary file, a sync, and a rename. Opening takes the
newest valid copy and rewrites the other one when it is damaged, missing, or
a generation behind — a crash between the two copy writes leaves exactly
that. Without the repair, one later fault in the newer copy would roll the
value back to one the caller may already have acted past: for an acceptor, a
promise going backwards. `Recovery::meta_repaired` reports it.

## Truncating

Suffix truncation zeroes the discarded slots, syncs, zeroes the discarded
entries, and syncs again before returning. Slots go first so that a crash
part-way leaves only entries without identifiers past the cut — kept as a
prefix of the old log, or read as its end — never an old identifier beside a
zeroed entry, which would look like corruption.

## Two Fault Models

The paper assumes what most disks promise: a crash leaves each unsynced
sector with its old contents or its new ones. Under that model, rewriting the
acknowledged bytes that share the block a batch starts in is harmless.

The simulator can be harsher. Following FoundationDB's `AsyncFileNonDurable`,
it can lose, garble, or shear a sector that was being rewritten, destroying
bytes a sync had already made durable. A byte-contiguous log rewrites exactly
such sectors, so on that disk an acknowledged entry can come back damaged.
The journal then does what the paper promises for any corruption: it reports
the entry, or refuses to start, and never returns wrong data.

## The Example Simulation

[`journal.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim-examples/src/journal.rs)
in `moonpool-sim-examples` (`cargo xtask sim run journal`) is the journal
inside a full simulation: one process owns it on its simulated disk, and
`Chaos::Attrition` crashes that process over and over — never wiping it,
since a wiped disk is data loss no local journal can recover from.

```rust,ignore
SimulationBuilder::new()
    .processes(1, || Box::new(JournalNode))
    .workload(JournalWorkload)
    .enable_chaos([Chaos::Attrition { /* 80% crash, no wipe */ }])
    .chaos_duration(Duration::from_secs(30))
    .set_iterations(50)
    .run()
```

The node's memory dies with it, so what it has acknowledged lives in a
`Ledger` published in the iteration's `StateHandle`. Every boot opens the
journal — running recovery — and checks it against that ledger before
writing again:

```rust,ignore
let (mut journal, recovery) = Journal::open(ctx.storage().clone(), "wal", config()).await?;
assert_always!(
    recovery.corrupt.iter().all(|id| id.index >= acked_end),
    "only unacknowledged entries are reported corrupt"
);
assert_always!(journal.next_index() >= acked_end, "no acknowledged entry is lost");
```

Then it appends batches, truncates suffixes and prefixes, and saves
metadata; a batch joins the ledger only once `append` returns. Moonpool's
default disk is the paper's model, so the promises are the strong ones. The
report shows what the crashes reached — torn tails, ambiguous last entries,
corrupt unacknowledged entries, rebuilt slots, several segments — and
removing the journal's per-batch `fdatasync` turns nearly every seed red.

## How It Was Tested

The integration tests drive the journal against the simulator's storage
directly. Targeted faults exercise each row of the table: a flipped payload
bit, a flipped slot, a zeroed slot, both at once, a flipped last entry, a
torn tail, an EIO block, and a damaged header. A crash loop runs a writer for
a random number of simulation steps, crashes the process with
`simulate_crash_for_process`, and reopens, playing the replication layer by
cutting the log at the first unreadable entry. Every recovery replays the
log twice — one `read` per index and one `read_range` — and the two must
agree; every corrupt entry must be reported with the tag it was written
with; half the seeds keep the ambiguous last entry instead of truncating
it; and the writer saves metadata, whose reopened value must be the last
acknowledged one or the one in flight. It runs under both fault models:

- **the paper's**: every acknowledged entry survives, and none is ever
  reported corrupt;
- **the simulator's full physics**: a read never returns anything but what
  was acknowledged — damage is always reported.

Removing the per-batch `fdatasync` turns the first loop red at its first
seed; removing the post-recovery clean-up turns the torn-tail test red;
removing the metadata repair turns both metadata tests red.
