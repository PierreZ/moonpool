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

CLSTORE stores each entry's identifier (index, epoch, offset, length, CRC) in
a **slot**, in a table at the front of the segment, megabytes away from the
entry. A failed entry with an intact slot was written and acknowledged; a
failed entry without one was not.

## Layout

```text
Offset   Region        Contents
0 KiB    Header A      magic, version, first_index,
4 KiB    Header B      slot_count, data_start, crc
8 KiB    Slot table    65,536 slots × 32 B (2 MiB)
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
| bad   | valid     | mark corrupt, report (index, epoch) upward |
| bad   | empty     | torn tail: truncate here                   |
| bad   | bad       | double fault: refuse to start              |

The first entry without an identifier ends the log; every earlier faulty
entry that has one is corruption. The one exception is the paper's theorem:
the *last* entry, slot present and entry bad, is exactly what a crash between
the slot write and the sync leaves, so nothing local can tell it from
corruption. A replication layer keeps it if committed and discards it if not;
the single-node journal truncates it and reports it as `ambiguous_tail`.

An EIO is zero-filled into a checksum mismatch, as in the paper, so an
unreadable entry is reported corrupt rather than stopping the journal. And
recovery cleans up after itself like any truncation: the discarded slots and
entry bytes are zeroed and synced before the first append.

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
    recovery.corrupt.iter().all(|(index, _)| *index >= acked_end),
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
cutting the log at the first unreadable entry. It runs under both fault
models:

- **the paper's**: every acknowledged entry survives, and none is ever
  reported corrupt;
- **the simulator's full physics**: a read never returns anything but what
  was acknowledged — damage is always reported.

Removing the per-batch `fdatasync` turns the first loop red at its first
seed; removing the post-recovery clean-up turns the torn-tail test red.
