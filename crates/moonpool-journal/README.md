# moonpool-journal

A write-ahead journal that implements CLSTORE, the local storage layer of
*Protocol-Aware Recovery for Consensus-Based Storage* (Alagappan et al.,
FAST '18): it detects damage, tells a crash apart from corruption, and names
what was damaged, so a replication layer can repair it from peers instead of
truncating it away.

## Why

A log that finds a bad checksum usually has one move: truncate there. That is
right for a torn write at the tail and silently wrong for an acknowledged
entry that rotted: truncating it can erase committed data cluster-wide. CLSTORE
stores every entry's identity in a persist record far from the entry: when the
entry fails its checksum, the record says it was written, and which entry it
was.

## What

| Type | Role |
|------|------|
| `Journal<P>` | Create, open (recover), commit, read, over any `StorageProvider` |
| `Batch` | What one commit makes durable: `put`, `clear`, `truncate_prefix`, `set_meta` |
| `JournalConfig` / `Durability` / `Geometry` | One or two syncs per commit; the segment shape |
| `Id` / `JournalId` | The caller's 24-byte identity per entry; the journal's own identity |
| `State` / `Recovery` | What a position holds (`Live`, `Corrupt`, `Ambiguous`); what opening found |
| `OpenError` / `CommitError` / `ReadError` | Operating errors vs. evidence of damage |

An entry is a `u64` **position** and an opaque `Id`; the journal knows no
protocol. Positions are written in any order, with gaps and overwrites (a
lagging replica fills old positions while new ones arrive); payloads go to an
arrival-order entry log, so writes stay sequential. `clear` removes a range,
`truncate_prefix` raises the floor and frees whole segments from the front.
The caller's node-unique state (a promise, a vote) is up to 4 KiB of
metainfo, in two copies. No snapshots, no compaction.

`<dir>/meta` holds the metainfo, copy A at block 0 and copy B at block 16,
written one after the other so at most one is ever in flight. Each
`<dir>/seg-<first batch>.wal` is preallocated and zero-filled: two header
copies, a persist log of 64-byte records, a guard gap, and an entry log.
Every batch starts on a fresh block in both logs, so no write ever touches a
sector holding anything acknowledged.

`Durability::Batched` is the paper's protocol (`write(entries);
write(records); fsync`): one sync, and the last batch's damaged entries are
`Ambiguous`, which the paper proves cannot be decided locally.
`Durability::Ordered` syncs in between: two syncs, and damage is always
told apart.

## Testing

`tests/journal.rs` runs the journal on the simulator's storage: positions in
any order, the floor and tombstones, one test per row of the recovery table
aimed with `Journal::layout`, the metainfo copies, lost batches and segments,
and crash loops under two fault models (the paper's sector-atomic one, and
moonpool's full physics: lost, latent and shorn sectors) for each durability
mode and for a mode drawn afresh every round (one test each), with crashes
inside recovery too. `JOURNAL_CRASH_SEEDS` raises the seed count,
`JOURNAL_CRASH_SEED` replays one.

## Examples

`examples/accept_log.rs` runs the journal on the real filesystem through
`TokioStorageProvider`: a Paxos acceptor stores accepts at their slots in
arrival order, restarts, finds one rotted accept reported with its ballot,
and repairs it as if from a peer:

```sh
cargo run -p moonpool-journal --example accept_log
```

`crates/moonpool-sim-examples/src/journal.rs` runs the journal inside a full
simulation, one node crashed over and over by attrition:

```sh
cargo xtask sim run journal
```
