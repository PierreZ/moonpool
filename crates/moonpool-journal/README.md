# moonpool-journal

A write-ahead journal that implements CLSTORE, the local storage layer of
*Protocol-Aware Recovery for Consensus-Based Storage* (Alagappan et al.,
FAST '18). It detects damage, tells a crash apart from corruption, and names
what was damaged. A replication layer can then repair the entry from its
peers instead of truncating the log.

The journal uses only moonpool's local provider traits, which every `Send`
provider also implements. The same code runs over `TokioStorageProvider` in
production, over `CompioStorageProvider` on a thread-per-core runtime, and
over `SimStorageProvider` in simulation. Over a `Send` provider the journal
and its futures are `Send`.

## Why

A log that finds a bad checksum usually truncates there. That is correct for
a torn write at the tail. It is wrong for an acknowledged entry that rotted:
truncation can erase committed data across the whole cluster. CLSTORE keeps
the identity of every entry in a persist record far from the entry. When the
entry fails its checksum, the record proves that it was written and says
which entry it was.

## Quick start

```rust
use moonpool_core::TokioStorageProvider;
use moonpool_journal::{Batch, ID_SIZE, Journal, JournalConfig, JournalId};

let provider = TokioStorageProvider::new();
let config = JournalConfig::default();
let mut journal = match Journal::open(provider.clone(), "wal", JournalId(7), config).await? {
    Some((journal, _recovery)) => journal,
    None => Journal::create(provider, "wal", JournalId(7), config, b"vote=none").await?,
};

// Positions go in any order; the metainfo lands after the entries.
let mut batch = Batch::new();
batch.put(9, [3; ID_SIZE], "set y=9").put(4, [3; ID_SIZE], "set x=4").set_meta("vote=3");
journal.commit(batch).await?;
```

## Features

| Type or method | Role |
|---|---|
| `Journal<P>` | `create`, `open` (recover), `commit`, `read`, `replay`, over any `LocalStorageProvider` (every `StorageProvider` is one); `Send` when `P` is |
| `Journal::rolls` / `Rolls` | How this open's rolls went: onto a recycled segment, or onto a newly allocated one |
| `Journal::peek_meta` | Read a closed journal's metainfo without recovery, repair or sync |
| `Batch` | What one commit makes durable: `put`, `clear`, `truncate_prefix`, `set_meta` |
| `Batch::fits` / `Batch::fits_another` | Whether a batch fits one segment, so the caller can split it |
| `JournalConfig` / `Durability` / `Geometry` | One or two syncs per commit; the segment shape; direct I/O |
| `Id` / `JournalId` | The caller's 24-byte identity per entry; the journal's own identity |
| `State` / `Recovery` | What a position holds (`Live`, `Corrupt`, `Ambiguous`); what opening found and repaired |
| `Journal::layout` / `Journal::regions` / `Layout` | Byte ranges of records, entries, metainfo and headers, for aiming faults |
| `OpenError` / `CommitError` / `ReadError` / `Durable` | Operating errors versus evidence of damage |

- **Positions in any order.** An entry is a `u64` position and an opaque
  `Id`. Positions are written in any order, with gaps and overwrites. A
  lagging replica fills old positions while new ones arrive. Payloads go to
  an arrival-order entry log, so the writes stay sequential.
- **Tombstones and a floor.** `clear` removes a range. `truncate_prefix`
  raises the floor and frees whole segments from the front.
- **Metainfo.** The caller's node-unique state (a promise, a vote) is up to
  4 KiB, in two copies. A crash can leave a batch durable without its
  metainfo, never the metainfo without its batch.
- **Two protocols.** `Durability::Batched` is the paper's protocol (`write
  entries; write records; fsync`): one sync, and the last batch's damaged
  entries are `Ambiguous`. `Durability::Ordered` syncs in between: two syncs,
  and damage is always told apart. Ordered issues each log's write as one
  durable write (`StorageFile::write_at_durable`, `pwritev2(RWF_DSYNC)` on
  Linux), two calls where a write and an `fdatasync` each were four; so do the
  metainfo copies and a roll's segment headers. A Batched commit whose sync
  fails writes its batch again before it retries (fsyncgate), at most three
  syncs in all.
- **No copy on the way to the disk.** A commit encodes its entries and
  records in place, in block-aligned `AlignedBuf`s, and moves them through
  the zero-copy `StorageFile::write_at_aligned` /
  `write_at_durable_aligned`: a direct-I/O segment never copies them. So do
  the headers, the metainfo copies and recovery's reads.
- **Recycled segments, no zero-fill.** A segment with nothing live left
  becomes the spare, and the next roll adopts it with a header write and a
  rename: no byte written, no space allocated. With no spare, a roll
  allocates a new segment (`StorageFile::allocate`, `fallocate(2)` on
  Linux) and writes only its headers. No segment is ever zero-filled, and
  no task runs beside the commits.
- **Crash hints.** A batched commit names three moments with
  `moonpool_buggify::hint!`; an ordered commit names the metainfo one and
  leaves the windows inside its two durable writes to the storage layer,
  which the simulator names "storage: durable write landed, not synced". They are no-ops in production. In a simulation,
  the simulator may reboot the process at one of them, under the seed's
  attrition regime and an optional `HintVeto` budget from the harness.

No snapshots and no compaction.

## On disk

`<dir>/meta` holds the metainfo: copy A at block 0, copy B at block 16. Each
`<dir>/seg-<first batch>.wal` is allocated whole: two header copies, a
persist log of 64-byte records, a guard gap, and an entry log. Every batch
starts on a fresh 4 KiB block in both logs, so no write touches a sector that
holds acknowledged data.

`<dir>/spare-<generation>-<n>.wal` is a dead segment's file, renamed and kept
for the next roll. A roll writes and syncs its headers, renames it to its
segment name and syncs the directory. Its old batches stay in it, and they
are never evidence: a segment named after its first batch only receives
batches at or above that number, batch numbers never repeat, so every record
or entry older than the segment reads as stale, like a never-written one. An
open keeps one whole spare and deletes the others.

A new segment reads as zeros without a byte written. Its first pass pays one
small metadata change per block on each durable write (the extent goes from
unwritten to written); a recycled segment pays none.

## Feature flags

None. The crate depends on `moonpool-core` with `default-features = false`.
Enable `moonpool-core/tokio-fs` in your application to use the real
filesystem.

## Testing

`tests/journal.rs` runs the journal on the simulator's storage:

- positions in any order, the floor and tombstones, the metainfo copies;
- one test per row of the recovery table, aimed with `Journal::layout`;
- lost batches and lost segments;
- crash loops under two fault models (the paper's sector-atomic model, and
  moonpool's full model with lost, latent and shorn sectors), for each
  durability mode and for a mode drawn again every round, with crashes
  inside recovery, floor raises that recycle segments, and rolls onto
  recycled segments.

`JOURNAL_CRASH_SEEDS` raises the seed count. `JOURNAL_CRASH_SEED` replays one
seed.

## Examples

`examples/accept_log.rs` runs the journal on the real filesystem. A Paxos
acceptor stores accepts at their slots in arrival order, restarts, finds one
rotted accept reported with its ballot, and repairs it as if from a peer:

```sh
cargo run -p moonpool-journal --example accept_log
```

`examples/bench.rs` measures commits on the real filesystem. Each writer
owns one journal and commits back to back for a fixed duration. It reports
throughput, commit latency percentiles and fairness between writers, and
`--csv` writes a time series with the page-cache dirty bytes.
`--segment-blocks N` shrinks the segments so they roll often,
`--truncate-every N` raises the floor so segments are recycled, and the
report gives the latency of the commits that rolled, onto a new segment and
onto a recycled one. Run it in
release mode on the disk under test (the default directory is under
`target/`; tmpfs is refused), and never compare numbers across `--fsync`
policies:

```sh
cargo run --release -p moonpool-journal --example bench -- \
    --writers 4 --record-size 4096 --batch 1 --fsync ordered --duration 10
```

`examples/backends.rs` runs the same journal on Tokio and on compio, on the
real filesystem: sequential durable appends at 128 B, 4 KiB and 64 KiB,
concurrent writers, and the recovery of a 256 MiB journal, buffered and with
direct I/O. It prints the environment and the results as Markdown. The
results of one run, with their caveats, are in
[`docs/benchmarks/journal-backends.md`](../../docs/benchmarks/journal-backends.md):

```sh
cargo run --release -p moonpool-journal --features compio-providers \
    --example backends -- --duration 5
```

`crates/moonpool-sim-examples/src/journal.rs` runs the journal inside a full
simulation, with one node crashed over and over by attrition:

```sh
cargo xtask sim run journal
```

## Documentation

- [API documentation](https://docs.rs/moonpool-journal)
- [A Crash-Aware Journal](https://pierrez.github.io/moonpool/part5-building-on-top/08-journal.html) in the Sim Book
