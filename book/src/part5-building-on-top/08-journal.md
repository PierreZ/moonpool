# A Crash-Aware Journal

<!-- toc -->

`moonpool-journal` is a write-ahead log built on nothing but the provider
traits and [`BlockFile`](../part2-foundations/07-provider-traits.md). It is
the kind of storage engine the simulator's disk model exists to break, and it
doubles as a worked example of testing one against that model.

## The Problem It Solves

A log that finds a bad checksum usually truncates there. For a torn write at
the tail that is correct: the entry was never acknowledged. For an entry that
was acknowledged and later rotted, it silently throws away committed data —
and everything after it. The two look identical if all the log has is the
entry.

The journal follows CLSTORE, the local storage layer of *Protocol-Aware
Recovery for Consensus-Based Storage* (FAST '18): each entry's identifier
(index, epoch, offset, length, CRC) lives in a **slot**, in a table at the
front of the segment, megabytes away from the entry. A failed entry with an
intact slot was written and acknowledged; a failed entry without one was not.

## Layout

```text
Offset   Region        Contents
0 KiB    Header A      magic, version, first_index, slot_count,
4 KiB    Header B      data_start, segment_size, crc
8 KiB    Slot table    65,536 slots × 32 B (2 MiB)
~2 MiB   Guard gap     zeros
4 MiB    Data region   append-only entries → end (64 MiB)
```

Segments are preallocated with real zeros, so the file size never changes
(`fdatasync` has no metadata to write, and a wrong size is itself a fault).
Log indexes are dense and slots fixed-size, so slot *i* sits at a computed
offset and the slot table is a plain array.

## Appending

Per batch: one write for the entries, one for their slots, one `fdatasync`.
Nothing orders the two writes; recovery does the disambiguation instead. Each
batch starts on a block boundary, so an append never rewrites a block that
holds acknowledged entries — `BlockFile` transfers whole blocks, and a shared
block would otherwise put synced bytes back into the crash window.

## Recovering

Opening walks every index. An entry is located through its slot, or — when
the slot is unusable — from the end of the previous entry or the next block
boundary. Then:

| Entry | Slot      | Before the last batch             | In the last batch            |
|-------|-----------|-----------------------------------|------------------------------|
| good  | valid     | keep                              | keep                         |
| good  | empty/bad | keep, rewrite the slot            | keep, rewrite the slot       |
| bad   | valid     | corrupt: report `(index, epoch)`  | ambiguous: truncate, report  |
| bad   | empty     | refuse: an entry went missing     | torn write: truncate         |
| bad   | bad       | refuse: double fault              | torn write: truncate         |

The *last batch* matters because only one batch is ever unsynced, and the
simulator's crash model resolves each of its sectors independently: entry *i*
can be torn while entry *i + 1* survived. Treating that hole as mid-log damage
would refuse to start after an ordinary crash. Each entry records its position
in its batch, so recovery knows where the last batch began.

## Truncating

Suffix truncation zeroes the discarded slots and entries before returning, so
new appends can never sit beside stale identifiers. But zeroing is many
writes, and a crash in the middle leaves any mix of zeroed and surviving
sectors — which reads exactly like mid-log damage. The cut is therefore made
durable first, as a fence in the manifest; recovery discards everything past a
fence whatever state it is in.

## How It Was Tested

The integration tests drive the journal with the simulator's storage directly
(no processes, no network): targeted bit flips for each row of the table, and
a crash loop that runs a writer for a random number of simulation steps,
crashes the process with `simulate_crash_for_process`, and reopens. Crash
severity is drawn per seed — lost, shorn, latent-fault and correlated-rollback
probabilities — because a brutal crash rarely leaves a later entry of the torn
batch intact and a mild one rarely tears anything; the holes live in between.
The invariant is two-sided: every acknowledged entry survives unchanged, and
no crash is ever reported as corruption.

Each rule above was confirmed load-bearing by removing it and watching the
loop go red: the per-batch `fdatasync` (acknowledged entries lost), the
last-batch tail rule (a crash reported as corruption), the truncation fence
(a crash mid-truncation refused as a missing entry), and the post-recovery
scrub (a stale entry resurrected under a new index).
