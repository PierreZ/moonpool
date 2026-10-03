# moonpool-journal

A write-ahead journal over moonpool's `BlockFile` that tells a crash apart
from corruption.

## Why

A write-ahead log that finds a bad checksum usually has one move: truncate
there. That is right for a torn write at the tail and silently wrong for an
acknowledged entry that rotted in the middle of the log. CLSTORE, from
*Protocol-Aware Recovery for Consensus-Based Storage* (Alagappan et al.,
FAST '18), fixes this by storing every entry's identifier in a slot far from
the entry: when the entry fails its checksum, its slot says whether it was
ever completely written, and which entry it was — its index, its epoch, and a
24-byte caller tag (a Paxos acceptor puts its slot and ballot there).

## What

| Type | Role |
|------|------|
| `Journal<P>` | Append, read, and truncate a segmented log over any `StorageProvider` |
| `JournalConfig` / `Geometry` | Segment shape and direct-I/O policy |
| `Record` / `Entry` / `EntryId` | What is appended, read back, and reported: index, epoch, tag, payload |
| `AmbiguousTail` | Truncate the damaged entries of the last batch (single node) or keep them for a replication layer |
| `Recovery` | What opening found: corrupt entries, the ambiguous last batch, repairs |
| `JournalError` | Operating errors vs. evidence of damage (`Corrupt`, `DoubleFault`, …) |
| `JournalAtlas` / `JournalRegion` | Where every region recovery reads lives, from an open journal or a closed directory, as format-neutral `LayoutRegion`s: aim faults by layout |

Each segment is one preallocated, zero-filled file (64 MiB by default) named
after its first index, found by listing the directory: two header copies, a
slot table (32,768 × 64 B), a guard gap, and a data region of contiguous
entries. A batch costs two writes and one `fdatasync`, and its first entry
carries a batch-start flag: a crash can tear any entry of the last batch, so
recovery reports that batch's damage as ambiguous rather than as corruption.
`read_range` replays a
range in large sequential reads. Term/vote metadata lives in `meta.0` /
`meta.1`, and opening repairs a damaged or stale copy from its twin.

## Testing

The integration tests run the journal on the simulator's storage: targeted
faults for each row of the recovery table, and a crash loop under two fault
models — the paper's (sector-atomic crashes: nothing acknowledged may be lost
or reported corrupt) and moonpool's full physics (a read may never return
wrong data). `JOURNAL_CRASH_SEEDS` raises the seed count;
`JOURNAL_CRASH_SEED` replays one. `tests/journal_atlas.rs` aims damage by the
`JournalAtlas` (the last batch, an entry beside its own slot, both copies of a
twin) and checks the verdict the damaged regions predict;
`JOURNAL_ATLAS_SEEDS` raises its seed count.

## Examples

`examples/accept_log.rs` runs the journal on the real filesystem through
`TokioStorageProvider`: a Paxos acceptor journals its accepts tagged with
their slot and ballot, restarts and replays them, then finds one entry
rotted on disk — reported corrupt with its Paxos identity instead of
truncated — and repairs it as if from a peer:

```sh
cargo run -p moonpool-journal --example accept_log
```

`crates/moonpool-sim-examples/src/journal.rs` runs the journal inside a full
simulation — one node crashed over and over by attrition, checking on every
boot that nothing it acknowledged was lost, changed, or reported corrupt:

```sh
cargo xtask sim run journal
```
