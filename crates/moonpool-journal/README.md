# moonpool-journal

A write-ahead journal over moonpool's `BlockFile` that tells a crash apart
from corruption.

## Why

A write-ahead log that finds a bad checksum usually has one move: truncate
there. That is right for a torn write at the tail and silently wrong for an
acknowledged entry that rotted in the middle of the log. The CLSTORE layout
from *Protocol-Aware Recovery for Consensus-Based Storage* (Alagappan et al.,
FAST '18) fixes this by storing every entry's identifier in a slot far from
the entry: when the entry fails its checksum, its slot says whether it was
ever completely written, and which entry (and epoch) it was.

## What

| Type | Role |
|------|------|
| `Journal<P>` | Append, read, and truncate a segmented log over any `StorageProvider` |
| `JournalConfig` / `Geometry` | Segment shape, batch bound, direct-I/O policy |
| `Recovery` | What opening found: corrupt entries, the ambiguous tail, repairs |
| `JournalError` | Operating errors vs. evidence of damage (`Corrupt`, `DoubleFault`, …) |

Each segment is one preallocated, zero-filled file (64 MiB by default) with
two header copies, a slot table (65,536 × 32 B), a guard gap, and an
append-only data region. A batch costs two writes and one `fdatasync`.

## Testing

The integration tests run the journal on the simulator's storage: targeted
bit flips in entries, slots, headers and metadata, and a randomized crash loop
that crashes a writer mid-append and mid-truncation under the full
barrier-bounded crash model, checking that no acknowledged entry is ever lost
and no crash is ever reported as corruption. `JOURNAL_CRASH_SEEDS` raises the
seed count; `JOURNAL_CRASH_SEED` replays one.

See the crate docs and the book chapter *A Crash-Aware Journal* for the layout
and the recovery rules.
