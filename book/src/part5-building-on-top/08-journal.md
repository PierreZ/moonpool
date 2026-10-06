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

Each segment is one preallocated file named after its first index; the
journal finds them with `StorageProvider::list_dir`. Writing every byte up
front means `fdatasync` never has metadata to write, and a file of the wrong
size is itself a detectable fault. Log indexes are dense and slots
fixed-size, so slot *i* sits at a computed offset and the slot table is a
plain array.

The slot table is not left as zeros. Every slot is formatted with a
**reserved record**: its own index, a reserved flag, and a CRC over the lot,
TigerBeetle's reserved headers. "No entry here" is then a positive,
checksummed statement, and a slot that comes back as zeros (a lost sector, a
zero-filled EIO, a misdirected page of zeros) is damage like any other. A
reserved record naming another index is a misdirected write. The CTRL paper's
Figure 2 bug lives exactly in that gap: if zeros mean "never written", a
synced identifier that comes back zeroed beside a damaged entry reads as the
end of the log, and every acknowledged entry after it is silently cut.

## Appending

Per batch: write the entries contiguously into the data region, write their
slots in one call, `fdatasync` once, acknowledge. Nothing orders the first
write before the second, so it is one sync per batch instead of two, with
recovery doing the disambiguation.

Every batch starts on a **fresh block**. The entries of one batch are packed
back to back, but the batch itself begins at the first 4 KiB boundary after
the previous one and pads its last block with zeros. That costs space, up to
a block per batch, and buys something a byte-contiguous log cannot have: the
data write never touches a sector an earlier sync made durable. A disk is
allowed to garble the sectors it was writing when the power went (more on
that under *Two Fault Models*), and with fresh blocks those sectors only ever
hold the batch nobody has acknowledged yet. TigerBeetle pads every prepare to
its own sectors for the same reason, and FoundationDB's `DiskQueue` starts
every commit on a fresh page.

The slot table is the one place an append still rewrites acknowledged bytes:
64 slots share a block, and writing a new slot rewrites its neighbours. That
is fine, because a lost slot beside an intact entry is the recovery table's
easiest row. Recovery finds the entry anyway (right after its predecessor
when it continues a batch, on the next block when it starts one, as its
batch-start flag says) and rebuilds the slot from it.

## Recovering

Opening walks every index. Entry *i*'s offset comes from its slot, or, when
the slot is damaged, from the end of entry *i − 1*. Then:

| Entry | Slot     | Action                                          |
|-------|----------|-------------------------------------------------|
| any   | reserved | the log ends here                               |
| good  | valid    | keep                                            |
| good  | bad      | keep, rewrite slot                              |
| bad   | valid    | mark corrupt, report its identity upward        |
| bad   | bad      | in the last batch: torn tail, the log ends here |
| bad   | bad      | before it: double fault, refuse to start        |

The first entry without an identifier ends the log; every earlier faulty
entry that has one is corruption. The one exception is the paper's theorem,
which it states for the *last entry* and which batching widens to the *last
batch*. A batch's entries and slots go out in two unordered writes and one
`fdatasync`. A crash before that sync returns can land any subset of their
sectors: slot 7 on disk beside a torn entry 7, slot 9 beside an intact entry
9. Every such entry of the last batch is exactly what a crash leaves, and
nothing durable after it proves the sync returned, so nothing local can tell
it from corruption. Damage *before* the last batch is different: a later
sync covers it, so only corruption explains it.

To find the last batch, every entry and slot carries a **batch-start flag**
(format version 3), set on the first entry of each synced unit. A batch that
rolls over into a new segment is two such units. The flag is in the entry as
well as in the slot, so a rebuilt slot keeps it. The damaged entries of the
last batch are reported as `ambiguous_batch`, never as `corrupt`, and what
happens next is the caller's choice, `JournalConfig::ambiguous_tail`:

- `AmbiguousTail::Truncate` (the default) treats them as a torn write and
  truncates from the first of them, taking the batch's intact entries after
  it along. That is the only safe move for a single node, whose alternative
  is refusing to start.
- `AmbiguousTail::Keep` leaves them in the log, marked corrupt, for a
  replication layer to keep if committed and discard (with
  `truncate_suffix`) if not. Truncating would erase the only local evidence
  that the entries existed: one more crash before the caller acted on the
  report, and a Paxos acceptor could answer "nothing accepted here" for a
  vote it may have cast — the CTRL paper's Figure 2 bug.

Because the journal tracks batches itself, a caller never has to encode batch
numbers into its epochs to tell a torn batch from rot.

The same reasoning settles the last row. A crash before the last batch's sync
can tear an entry **and** its slot, since nothing orders the two writes. That
is a torn tail, not a double fault, so inside the last batch a damaged
identifier beside a damaged entry ends the log. Refusing to start there would
brick a node over a batch nobody acknowledged. Before the last batch the same
pair is a double fault, as the paper says: a later sync covered both copies.
An index is in the last batch when no identifier further on starts a batch
(identifiers past a reserved one are leftovers of a cut a crash interrupted,
and prove nothing).

An EIO is zero-filled into a checksum mismatch, as in the paper, so an
unreadable entry is reported corrupt rather than stopping the journal. And
recovery cleans up after itself like any truncation: the discarded slots get
their reserved records back and the discarded entries' blocks are zeroed,
synced, before the first append.

Recovery also **syncs what it keeps**. A process that restarts without a
power loss (a graceful reboot, a crash of the process alone) finds its
predecessor's unsynced writes still in the page cache, perfectly readable.
Recovery takes them into the log, and the caller acts on them: a Paxos
acceptor answers the next `Prepare` with the accept it finds there. If that
accept was never synced, the next power loss takes it back, after a peer
heard about it. PostgreSQL and RocksDB fsync their logs on recovery for the
same reason. The example simulation found this one, a graceful reboot
followed by a power loss.

## Replaying

`Journal::read_range(start..next)` is the replay a caller runs after
opening. Entries are packed back to back, so each segment's share of the
range comes back in large sequential reads instead of one read per entry,
with exactly `read`'s checks; a corrupt entry is returned in place as
`Err(EntryId)` and the replay goes on.

## Checkpoints

A caller that journals operations and folds them at boot (a Paxos acceptor
replaying its accepts, latest wins) has to compact: re-emit the folded state,
then drop the history it supersedes. Done by hand, every rule of that dance is
a rule about **batches**. Where does the image start and end? Is the last one
whole, or did a crash cut it? Which intact image is newest? The journal
already knows its batches, so it owns the dance.

`Journal::append_checkpoint(&records)` appends the state as one batch flagged
as a checkpoint. It is never split: if the current segment cannot hold it,
the journal rolls over first, so a checkpoint is always one batch and one
sync. Its first entry and slot also record **how many entries** it has, and
that number is what keeps a cut checkpoint honest. A crash before the sync
can land the first few entries and their slots and lose the rest to reserved
slots, leaving a shorter run of entries that all verify. Without the count,
that fragment would pass for a complete, smaller image.

`Recovery::checkpoint` (and `Journal::checkpoint`, at any time) names the
newest checkpoint whose every entry is in the log and intact. A damaged one is
reported in `corrupt` like any damaged batch and passed over for the one
before it, and a cut one is passed over too. A crash leaves a cut checkpoint
as the log's last batch, handled like any other. The caller replays from the
named one:

```rust,ignore
let (journal, recovery) = Journal::open(provider, "wal", config).await?;
let from = recovery.checkpoint.map_or(journal.start_index(), |c| c.start);
for entry in journal.read_range(from..journal.next_index()).await? {
    fold(entry?);
}
```

Dropping the history stays the caller's `truncate_prefix(checkpoint.start)`,
so a crash between the checkpoint and the drop is harmless: the old history
is still there, and a damaged newest checkpoint falls back to the one before
it, or to the whole history. When to checkpoint (every so many entries, or
when the history grows past some multiple of the image) is the caller's call
too.

## Metadata

Term/vote-style metadata (`save_meta`, `meta`) lives beside the segments in
two copies, `meta.0` and `meta.1`, each with a generation and a CRC, each
replaced through a temporary file, a sync, and a rename. Opening takes the
newest valid copy and rewrites the other one when it is damaged, missing, or
a generation behind — a crash between the two copy writes leaves exactly
that. Without the repair, one later fault in the newer copy would roll the
value back to one the caller may already have acted past: for an acceptor, a
promise going backwards. `Recovery::meta_repaired` reports it.

A save spends its generation *before* writing either copy, and keeps it
spent when it fails. A save can fail after both copies landed (the directory
sync after the second rename, say), and a caller whose promise moved on
retries with a newer value. Had the failed save not consumed its generation,
the retry would write the same one, and a crash after the retry's first copy
would leave two valid copies of one generation, the older value among them:
loading would pick one, and could pick the promise that went backwards. As it
is, the retry always outranks what the failure left, and two valid copies of
one generation that disagree can only be damage, so opening refuses them
with `MetadataCorrupt`.

Sometimes a caller only needs to look. A Paxos node keeps its format marker
in the metadata, and provisioning wants to know whether a store was ever
formatted **before** booting it. Opening is the wrong tool: it repairs
headers and slots, rewrites a stale copy, and may cut the last batch, so a
probe meant to look could change what the real boot finds.
`Journal::peek_meta(&provider, dir)` reads the newest valid copy and
nothing else. It repairs nothing, creates nothing (an absent directory reads
as `None`), and reports `MetadataCorrupt` when no copy is valid.

## Truncating

Suffix truncation resets the discarded slots to their reserved records,
syncs, zeroes the discarded entries' blocks, and syncs again before
returning. Slots go first so that a crash part-way leaves a reserved slot
past the cut, the end of the log, never an old identifier beside a zeroed
entry, which would look like corruption.

What it does **not** touch is the block the cut falls in. That block also
holds the entries being kept, and rewriting it would hand them to the next
crash, so the cut entries' bytes stay there, inert behind their reserved
slots. They do sit exactly where a continuation of the last kept entry would.
If the reserved slot after the cut were later damaged, the walk would look
there and find an intact entry with the right index. So the truncation also
marks the last kept slot **cut**, and the walk never takes anything after a
cut entry for a continuation of its batch. Recovery does the same when it
discards a torn tail.

Prefix truncation (compaction) deletes whole segments, and here the danger is
the directory, not the data. Unlinks are directory operations, and an unsynced
one can be undone by a crash, each name on its own. Delete `seg-1` and `seg-2`,
crash before the directory sync, and the disk may come back with `seg-1` and
without `seg-2`. Before it recorded anything, the journal would then read
`seg-1` as the start of the log. Its walk ran to the next segment's first
index, past `seg-1`'s real end, met the first empty slot, and took it for the
end of the log, deleting every later segment, acknowledged entries included.
If `seg-1` had rolled over because its slot table filled, the walk indexed
past the table instead and panicked.

So `truncate_prefix` first makes the new start durable, in its own two-copy
record (`start.0`, `start.1`, built exactly like the metadata), and only then
unlinks, oldest first. Opening reads the recorded start and deletes again any
segment wholly below it. A sealed segment must reach the next one's first
index exactly: if it ends short, a segment is missing, and opening refuses
with `SegmentGap` rather than discard what follows. Suffix truncation, which
deletes from the other end, syncs the directory after each unlink, so a crash
can undo at most the last one and the survivors are always a prefix.

Opening also syncs the journal's directory before trusting anything in it.
The crash loop found why. A segment renamed into place by a rollover whose
directory sync failed is visible but not durable. The caller reopens, the new
segment is there, appends are acknowledged into it, and the next crash takes
its name, and with it the whole log.

## Two Fault Models

The paper assumes what most disks promise: a crash leaves each unsynced
sector with its old contents or its new ones.

The simulator can be harsher. Following FoundationDB's `AsyncFileNonDurable`,
it can lose, garble, or shear a sector that was being rewritten, destroying
bytes a sync had already made durable. A byte-contiguous log rewrites exactly
such sectors on every append. This journal never rewrites a sector holding an
acknowledged entry: appends start on fresh blocks, and truncation and
recovery leave the block holding the cut alone. So on that disk too every
acknowledged entry survives a crash. Before fresh blocks, a crash that lost
the sectors of a one-entry batch sharing a block with the batch before it
took that earlier batch along, slots and all, and recovery read the whole log
as a torn tail.

The slot table is the exception, by design: writing a new slot rewrites its
block, which holds acknowledged identifiers too. A crash can lose those, and
it costs nothing, because each sits beside an intact entry that rebuilds it.
The harsher disk therefore earns the same promise as the paper's, and the
crash loop holds it to that promise.

## Aiming Faults at the Layout

The layout is known, so a fault injector does not have to damage a segment
uniformly, where most sectors are preallocated zeros nobody reads. An
`JournalAtlas` names every region recovery reads and where it lives, as a
file and a byte range:

| `JournalRegion` | What damage there makes recovery do |
|----------|-------------------------------------|
| `Header { segment, copy }` | repair it from its twin; both copies: `BadSegmentHeader` |
| `Slot(id)` | rebuild it from an intact entry; beside a damaged entry: `DoubleFault`, or a torn tail in the last batch |
| `Entry(id)` | report it corrupt, or ambiguous in the last batch |
| `Meta { copy }` | repair it from the other copy; both: `MetadataCorrupt` |

`Journal::atlas` charts an open journal from what its recovery scan
verified. `JournalAtlas::scan` charts a closed directory from its slot tables
alone, without opening it and so without repairing anything: an injector
damages the bytes before the next boot reads them. `JournalAtlas::last_batch` and
`JournalAtlas::starts_batch` give the batch boundaries, and `JournalAtlas::regions_in`
maps damaged bytes back to the regions they reach: one sector of the slot
table holds eight slots, one block several entries.

Every charted region also comes as a `LayoutRegion` from `moonpool-core`
(`JournalAtlas::layout`): a file, a byte range, and a kind label
(`JournalRegion::SLOT`, `ENTRY`, `HEADER`, `META`). That form is
format-neutral, so the simulator aims faults at it without knowing the
journal, and any other storage format can describe its own layout the same
way.

Because each region carries its meaning, the injector also knows what the
damage *should* do. The crate's `tests/journal_atlas.rs` aims damage the way a
recovery decision needs it: part of the last batch, an entry beside its
own slot, both copies of a twin, a run of slots. It predicts the verdict
from the regions hit by applying the recovery table, then checks that
reopening reports exactly that, and that a second reopen finds every
repair durable. Uniform damage almost never hits a pair like an entry and
its own slot. Aimed damage does: a few thousand seeds reach every verdict,
double faults included.

The atlas also aims the simulator's own random faults. After every write,
the crash loop's `Aimed` model hands `Journal::atlas().layout()` to the disk as a
`FaultFocus` (slots weighted 8, entries and twin copies 4, zeros 0.1), so
the crash physics damage identifiers and live entries rather than the
preallocated zeros. A crash can only damage the sectors dirty at that
moment: the batch being written, and the slot-table blocks its slots share
with acknowledged ones. Aimed, it hits those identifiers far more often. On
120 seeds the acknowledged identifiers recovery had to rebuild rose from 276
to 905, and the guarantee held throughout: nothing acknowledged was lost.

## The Example Simulation

[`journal.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim-examples/src/journal.rs)
in `moonpool-sim-examples` (`cargo xtask sim run journal`) is the journal
inside a full simulation: one process owns it on its simulated disk,
`Chaos::Attrition` crashes that process over and over (never wiping it,
since a wiped disk is data loss no local journal can recover from), and
`Chaos::Storage(Random)` runs the disk's fault families underneath.

```rust,ignore
SimulationBuilder::new()
    .processes(1, || Box::new(JournalNode))
    .workload(JournalWorkload)
    .enable_chaos([
        Chaos::Attrition { /* 80% crash, 20% graceful, no wipe */ },
        Chaos::Storage(ChaosMode::Random),
    ])
    // What a lone node has no second copy to repair from.
    .storage_fault_mask(
        StorageFaultMask::all()
            .without(StorageFault::Corruption)
            .without(StorageFault::Eio)
            .without(StorageFault::Misdirect)
            .without(StorageFault::PhantomWrite)
            .without(StorageFault::DiskFailure),
    )
    .chaos_duration(Duration::from_secs(30))
    .set_iterations(50)
    .run()
```

What stays on is everything a lone journal must survive: the crash physics
(lost, latent and shorn sectors, correlated rollbacks), lost directory
entries, failed syncs, short transfers, slow-disk episodes. A failed sync
poisons the journal, and the node reopens it, as any caller should. The
journal runs with `AmbiguousTail::Keep`, the replicated caller's policy, and
the node plays the replication layer by discarding what it kept.

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
assert_always!(recovery.corrupt.is_empty(), "a crash is never reported as corruption");
```

Then it appends batches, truncates suffixes and prefixes, and saves
metadata; a batch joins the ledger only once `append` returns. The journal
never rewrites a sector holding an acknowledged entry, so even on the
harsher disk the promises are the paper's strong ones. The report shows what
the crashes and the disk reached: torn tails, ambiguous last batches
(several damaged entries in one of them, too), rebuilt slots, several
segments, failed syncs and the reopens they forced. Removing the journal's
per-batch `fdatasync` turns nearly every seed red, and so did a missing
sync on recovery, which this example found: a graceful reboot reopened
over a batch that was never synced, the node adopted it, and the next power
loss took it back.

## On a Real Disk

The journal is written against the provider traits only, so production
hands it `TokioStorageProvider` instead of the simulator's storage.
[`accept_log.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-journal/examples/accept_log.rs)
(`cargo run -p moonpool-journal --example accept_log`) does exactly that, in
a temporary directory: a Paxos acceptor appends its accepts, each tagged with
the slot and ballot it is for, keeps its promise in the two-copy metadata,
and replays everything with one `read_range` after a restart. Then it flips
one byte of an acknowledged entry in the middle of the log, behind the
journal's back:

```rust,ignore
let (journal, recovery) = Journal::open(TokioStorageProvider::new(), dir, config()).await?;
for entry in journal.read_range(journal.start_index()..journal.next_index()).await? {
    match entry {
        Ok(entry) => replay(entry),
        // Not truncated: the slot still names the Paxos slot and ballot.
        Err(id) => fetch_from_peer(decode_tag(&id.tag)),
    }
}
```

The reopen keeps every later entry and reports the damaged one with the
identity its slot recorded, so the acceptor cuts the log there with
`truncate_suffix`, re-appends the peer's copy and the entries after it, and
a final reopen comes back clean.

## How It Was Tested

The integration tests drive the journal against the simulator's storage
directly. Targeted faults exercise each row of the table: a flipped payload
bit, a flipped slot, a zeroed slot, both at once, a flipped last entry, a
torn tail, an EIO block, and a damaged header. The last-batch rule gets its
own cases: several damaged entries of one batch, the same batch kept, damage
before the last batch that stays corruption, and a lost batch-start slot
that is rebuilt with its flag. A crash loop runs a writer for
a random number of simulation steps, crashes the process with
`simulate_crash_for_process`, and reopens, playing the replication layer by
cutting the log at the first unreadable entry. Every recovery replays the
log twice — one `read` per index and one `read_range` — and the two must
agree; every corrupt entry must be reported with the tag it was written
with; half the seeds keep the ambiguous last batch instead of truncating
it; and the writer saves metadata, whose reopened value must be the last
acknowledged one or the one in flight. A third of the seeds checkpoint: the
writer folds its records into a state, re-emits it with `append_checkpoint`,
and drops its prefix only behind the newest checkpoint, and every recovery
checks that replaying from `Recovery::checkpoint` gives exactly the state the
whole history gives, and that once the prefix is gone a checkpoint is always
there. Accepting a checkpoint cut short, ignoring its count, turns that red.
It runs under both fault models:

- **the paper's**: every acknowledged entry survives, and none is ever
  reported corrupt or ambiguous. Nothing at all lands in `corrupt`: every
  torn entry is in the ambiguous last batch;
- **the simulator's full physics** (crash-lost sectors up to 0.3, latent
  faults up to 0.1, shorn writes up to 0.2, garbage fills): exactly the same
  promise, and every boot opens. Lost acknowledged identifiers are rebuilt
  from their entries.

Removing the per-batch `fdatasync` turns the first loop red at its first
seed; removing the post-recovery clean-up turns the torn-tail test red;
removing the metadata repair turns both metadata tests red.
