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
ever completely written, and which entry (and epoch) it was.

## What

| Type | Role |
|------|------|
| `Journal<P>` | Append, read, and truncate a segmented log over any `StorageProvider` |
| `JournalConfig` / `Geometry` | Segment shape and direct-I/O policy |
| `Recovery` | What opening found: corrupt entries, the ambiguous last entry, repairs |
| `JournalError` | Operating errors vs. evidence of damage (`Corrupt`, `DoubleFault`, …) |

Each segment is one preallocated, zero-filled file (64 MiB by default) named
after its first index, found by listing the directory: two header copies, a
slot table (65,536 × 32 B), a guard gap, and a data region of contiguous
entries. A batch costs two writes and one `fdatasync`. Term/vote metadata
lives in `meta.0` / `meta.1`.

## Testing

The integration tests run the journal on the simulator's storage: targeted
faults for each row of the recovery table, and a crash loop under two fault
models — the paper's (sector-atomic crashes: nothing acknowledged may be lost
or reported corrupt) and moonpool's full physics (a read may never return
wrong data). `JOURNAL_CRASH_SEEDS` raises the seed count;
`JOURNAL_CRASH_SEED` replays one.

## Example

`crates/moonpool-sim-examples/src/journal.rs` runs the journal inside a full
simulation — one node crashed over and over by attrition, checking on every
boot that nothing it acknowledged was lost, changed, or reported corrupt:

```sh
cargo xtask sim run journal
```
