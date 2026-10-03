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

## Aiming Faults at the Layout

The layout is known, so a fault injector does not have to damage a segment
uniformly, where most sectors are preallocated zeros nobody reads. An
`JournalAtlas` names every region recovery reads and where it lives, as a
file and a byte range:

| `JournalRegion` | What damage there makes recovery do |
|----------|-------------------------------------|
| `Header { segment, copy }` | repair it from its twin; both copies: `BadSegmentHeader` |
| `Slot(id)` | rebuild it from an intact entry; beside a damaged entry: `DoubleFault` |
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
moment, so the gain is bounded, but it is real: on 250 seeds the damage
reported against acknowledged data rose from 40 to 67, and refusals from 1
to 4. The guarantee is unchanged: a read never returns wrong data.

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
assert_always!(recovery.corrupt.is_empty(), "a crash is never reported as corruption");
```

Then it appends batches, truncates suffixes and prefixes, and saves
metadata; a batch joins the ledger only once `append` returns. Moonpool's
default disk is the paper's model, so the promises are the strong ones. The
report shows what the crashes reached — torn tails, ambiguous last batches
(several damaged entries in one of them, too), rebuilt slots, several
segments — and
removing the journal's per-batch `fdatasync` turns nearly every seed red.

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
acknowledged one or the one in flight. It runs under both fault models:

- **the paper's**: every acknowledged entry survives, and none is ever
  reported corrupt or ambiguous. Nothing at all lands in `corrupt`: every
  torn entry is in the ambiguous last batch;
- **the simulator's full physics**: a read never returns anything but what
  was acknowledged — damage is always reported.

Removing the per-batch `fdatasync` turns the first loop red at its first
seed; removing the post-recovery clean-up turns the torn-tail test red;
removing the metadata repair turns both metadata tests red.
