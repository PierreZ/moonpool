# Aiming Storage Faults

<!-- toc -->

## Most of a Disk Is Nobody's Business

Random storage faults are layout-blind. A corruption coin lands on whatever sector an operation touches, and a crash resolves every dirty sector alike. Most of a preallocated journal is zeros that nobody reads. A uniform fault mostly lands there, the run stays green, and the sixty-four bytes that recovery has to judge, the identifiers of the last batch, are never hit.

Moonpool keeps its fault coordinates simple: **a file plus a flat sector offset**. It does not know what a slot, a page or a header is. The format does. So the format tells the simulator where its bytes live, and three mechanisms use that map: an eligibility mask, a fault focus and a replicated fault pattern. A fourth, targeted injection, needs no map at all.

## A Map the Format Draws

`LayoutRegion` in `moonpool-core` is plain data: one named byte range of one file.

```rust
pub struct LayoutRegion {
    pub path: String,         // as the format opens it
    pub bytes: Range<u64>,
    pub kind: &'static str,   // the format's own label: "slot", "header"...
    pub stripe: Option<u64>,  // the replicated record these bytes hold
}
```

`kind` lets a harness weigh kinds differently. `stripe` is the key every replica's copy of a record shares (a Paxos slot, a page number), or `None` for bytes only this node has, such as a header. Nothing in it depends on a simulator, so a format lists its regions without a `cfg(test)`.

`moonpool-journal` is one such format. `Journal::regions()` returns every region a fault could hit: each live position's persist record and entry, striped by position, plus the two metainfo copies and every segment's two header copies with no stripe. `Journal::layout(position)` returns the `Layout` of one position. The kinds are `Layout::RECORD`, `Layout::ENTRY`, `Layout::META` and `Layout::HEADER`. See [A Crash-Aware Journal](../part5-building-on-top/08-journal.md).

## The Eligibility Mask

The coarsest control is a veto: `(path, sector) -> bool`. `SimWorld::set_storage_eligibility_mask(mask)` installs one, and `clear_storage_eligibility_mask()` removes it. The random families and the damaging crash outcomes (`Lost`, `LatentFault`, `Shorn`) consult it. An ineligible sector falls back to the always-legal old-or-new resolution.

```rust
// Never damage sector 0 of any file: the superblock stays readable.
sim.set_storage_eligibility_mask(Arc::new(|_path: &str, sector: u64| sector != 0));
```

This is TigerBeetle's `ClusterFaultAtlas` pattern, used by a harness that keeps its own ledger. Coins are rolled **before** the mask is consulted, so installing one never shifts the random stream.

## Fault Focus

A mask says whether. A `FaultFocus` says **how likely**. It gives a weight to byte ranges of a process's files and a background weight to every other sector of that disk. Every family rolled per sector or per operation (read and write corruption, read and write EIO, phantom writes, sync lies, and the damaging crash outcomes) has its probability multiplied by the weight of the sectors it would hit:

```rust
// Inside a process: aim this disk's faults at the journal's live regions.
let focus = FaultFocus::new()
    .background(0.1) // the zeros, barely
    .layout(&journal.regions(), |region| {
        if region.kind == Layout::RECORD { 8.0 } else { 4.0 }
    });
ctx.storage().focus_faults(focus)?;
```

`spot(path, bytes, weight)` adds one range by hand. Where spots overlap, the heaviest wins. A weight of 0 makes a sector immune. A weight never revives a family the configuration leaves at zero, and a misdirected write honours only the immunity. The rolls are drawn whatever the weights, so a focus changes which faults fire, never which draws decide them.

`SimStorageProvider::focus_faults` is reachable from process code and scopes the focus to that process's disk. `clear_fault_focus()` removes it. A hand-driven test calls `SimWorld::set_fault_focus(ip, focus)`. A focus survives its process's crash and is cleared by a wipe of its storage.

## Targeted Injection

A directed test does not want probabilities. It wants **this** sector broken, now. Four `SimWorld` methods are path-global, reaching every process's file at that path:

| Method | Effect |
|--------|--------|
| `corrupt_file(path, sectors)` | Deterministic damage on those sectors |
| `fail_file_with_eio(path, sectors, target)` | Reads, writes or both (`EioTarget`) on those sectors fail with an I/O error |
| `clear_file_eio(path, target)` | Removes the injection |
| `corrupt_durable_out_of_band(path, sector)` | A deliberate barrier violation, to test the oracle itself |

To damage one copy of one record on one replica, aim at one owner and exact bytes:

| Method | Effect |
|--------|--------|
| `corrupt_process_file_bytes(ip, path, bytes)` | Durably changes exactly those bytes of that process's file. Each byte is XORed with a nonzero mask of its offset, so no randomness is drawn. The durability oracle is re-stamped, so a later crash does not report a lost synced write. |
| `fail_process_file_with_eio(ip, path, sectors, target)` | EIO on one owner's file only |
| `clear_process_file_eio(ip, path, target)` | Removes it |
| `process_file_durable_bytes(ip, path)` | That file's durable image, to find the bytes to aim at |

A scripted `FaultInjector` holds the same four on `FaultContext`: `durable_bytes`, `corrupt_bytes`, `fail_file_with_eio` and `clear_file_eio`, each taking the process IP as a string. Reading the durable bytes takes no simulated time, rolls no fault coin and draws no randomness, so aiming never perturbs the run:

```rust
// Inside a FaultInjector: damage three bytes of the first replica only.
let first = &ctx.process_ips()[0];
let image = ctx.durable_bytes(first, "data")?;
let record = find_last_record(&image); // the format's own parsing
ctx.corrupt_bytes(first, "data", record.start..record.start + 3)?;
```

The other replicas' files at `"data"` are untouched. `corrupt_bytes` refuses a range past the durable end with `StorageError::OutOfRange`, and a missing file with `NotFound`.

## Replicated Fault Patterns

A replicated system survives disk damage only while some replica keeps a clean copy of every record. Random faults drawn independently per disk eventually hit every copy of one record, and a valid run becomes unwinnable. A harness could keep a ledger. Moonpool prevents it by construction, because it knows the topology:

```rust
SimulationBuilder::new()
    .cluster(LocalityConfig::new(1, 3, 1..=2, 1), make_node)
    .enable_chaos([Chaos::Storage(ChaosMode::Swarm)])
    .replicated_storage_faults(ReplicatedFaults::new(DomainLevel::Zone).group("node"))
```

Each process publishes what its disk holds with `ctx.storage().publish_layout(&regions)`. The `stripe` of each region is the record's key, shared by every replica's copy. A `ReplicatedFaults` covers one process group (`.group(name)`, the processes' `Process::name`), or the whole topology without one. Call `replicated_storage_faults` once per replicated group, so acceptors and matchmakers each draw their own pattern. Processes outside every covered group keep plain storage chaos.

Each seed draws one `FaultPattern` per group over the failure domains at the chosen `DomainLevel` (`Machine`, `Zone` or `Datacenter`):

| `FaultPattern` | Where damage may land |
|----------------|-----------------------|
| `Minority { domains }` | Only the processes of a few domains, anywhere on their disks. TigerBeetle's minority corruption. |
| `Striped { domains, local }` | Every domain, but stripe `s` only in the domain it rotates to. TigerBeetle's helical corruption, keyed by **record** instead of file offset, because a replicated log writes the same record at different offsets on each replica. |
| `Rolling { domains }` | One domain at a time, anywhere on its disks. The turn moves on once the damage is repaired. |
| `Spared` | The group has fewer than two domains, so no process in it takes damage. |

`tolerance(n)` (default 1) is how many domains may hold damage to one record at once. It is clamped so one domain always keeps every record. Under a stripe rotation, node-local bytes (stripe `None`, or bytes never published) are damaged only in the `tolerance - 1` `local` domains, and those domains may damage every stripe. A format may answer a damaged header by giving up the whole replica, so node-local damage counts against every stripe. At the default tolerance no node-local byte is damaged under this pattern.

A rolling turn is repaired when every damaged sector has been rewritten, truncated away or deleted, or is no longer inside a region its process publishes. A domain holding no damage passes the turn as soon as another domain's disk is up for a fault. A domain that never repairs holds the turn, and a run stuck on one turn is the evidence. `SimWorld::fault_turn(ip)` names the domains holding the turn, and `SimStorageProvider::fault_pattern()` tells a process which pattern its group drew. `SimWorld::draw_replicated_faults(config, &localities)` draws a pattern for a hand-driven test.

Failure domains come from the [`.cluster()`](27-process-groups.md) topology. A group registered with `.processes()` has none and is always spared. The pattern shapes the same families a focus weighs. It is a weight of zero where damage may not land, and it composes with a focus and the mask. Disk failure, wipes and lost directory entries are outside it: they take a whole disk, or only unsynced state.

## Which One to Reach For

| You want | Use |
|----------|-----|
| A sector that must never break | The eligibility mask |
| Random faults where recovery has to look | `FaultFocus` with the format's `LayoutRegion`s |
| One record on one replica, broken now | `FaultContext::corrupt_bytes` after `durable_bytes` |
| A cluster that stays recoverable under random damage | `replicated_storage_faults` with `publish_layout` |

All four compose. The pattern and the mask veto first, the focus weighs what remains, and a targeted injection ignores all three because the test owns its budget.
