# Storage Faults

<!-- toc -->

## Disks Lie

`write()` returns success and the bytes never reach the platter. `fsync()` completes and the firmware never flushed its cache. A firmware bug sends a write to the wrong sector. TigerBeetle's documentation lists these with real incidents behind them, including studies where 8.5% of SATA drives developed silent corruption. Moonpool's storage faults follow TigerBeetle's taxonomy and FoundationDB's crash model, so your integrity code meets these failures in simulation, deterministically.

## The Storage Layering Rules

Before the faults, the API they land on. Moonpool has **one provider, one file abstraction, one simulated file image**. Every API (stream, positioned, block) reads and writes the same bytes.

| Layer | Owns | Methods |
|-------|------|---------|
| `StorageProvider` | The filesystem **namespace** | `open`, `exists`, `delete`, `rename`, `create_dir_all`, `list_dir`, `sync_dir` |
| `StorageFile` | Already-open **bytes** | `AsyncRead`/`AsyncWrite`/`AsyncSeek`, `read_at`, `write_at`, `write_at_durable` and their zero-copy `*_aligned` forms, `sync_all`, `sync_data`, `size`, `set_len`, `constraints`, `is_direct_io` |
| `BlockFile<F>` | Block arithmetic over one open file | `read_blocks`, `write_blocks`, `write_blocks_durable` and their `*_aligned` forms, `sync`, `sync_data`, `grow_to_blocks`, `size_in_blocks`, `buffer` |

Database needs are **options on opening an ordinary file**, never a second provider stack. There is no `BlockProvider`, no `DirectIoProvider` and no region-addressed device:

```rust
// A pager opens an existing file and demands uncached I/O.
let pages = provider
    .open("db/pages", OpenOptions::read_write().direct_io(DirectIo::Required))
    .await?;
let blocks = BlockFile::new(pages, 4096)?;
let mut page = blocks.buffer(1)?; // aligned for this file
blocks.read_blocks(7, page.as_mut_slice()).await?;
```

`DirectIo` has three policies. `Disabled` is buffered I/O. `Optional` asks for uncached I/O and accepts a fallback, and `file.is_direct_io()` tells you which one you got. `Required` fails the open instead of downgrading, and it **opens an existing file only**. Creating a file and publishing its name is a namespace protocol that belongs to the format, so a journal bootstraps its own file: create it with an ordinary open, `sync_all` it, `sync_dir` its parent, then reopen it with `Required`.

Keep four distinctions, because code hides behind their opposites:

1. **Direct I/O is not durability.** An uncached write is visible, not durable, until `sync_all` or `sync_data` completes, or unless it was a `write_at_durable`. A durable write covers its own range only, like `O_DSYNC`: it does not make an earlier unsynced write elsewhere in the file durable.
2. **The caller's block size is not the file's I/O alignment**, and neither is a crash-atomicity unit. `file.constraints()` reports offset, length and memory alignment separately. A two-block write tears across a crash like any other.
3. **Growing a file is not preallocation.**
4. **File durability is not directory-entry durability.** `sync_all` makes the bytes and the length survive. Only `StorageProvider::sync_dir(parent)` makes the **name** survive.

A file with alignment constraints refuses the stream API with `ErrorKind::Unsupported`, on both backends, because a shared cursor cannot stay aligned. `read_at` and `write_at` are what such a file offers. Both return the count moved, which may be short. `BlockFile` exists to loop over short transfers so a pager does not.

### Zero copy: owned buffers

Every positioned transfer has a borrowed form (`&[u8]`, `&mut [u8]`) and an owned form over an `AlignedBuf` (`read_at_aligned`, `write_at_aligned`, `write_at_durable_aligned`; on `BlockFile`, `read_blocks_aligned`, `write_blocks_aligned`, `write_blocks_durable_aligned`). They are one transfer with one contract. The owned form moves the buffer in and hands it back with the count, so the caller reuses it; an error drops it.

The difference is a copy. The Tokio backend runs positioned I/O on the blocking pool, and a borrowed slice cannot cross into a `'static` task: the borrowed form copies the bytes into an aligned bounce buffer before a write and out of it after a read. The owned form moves the caller's buffer into the task instead, so the kernel transfers straight from and into it. A storage engine that encodes its pages or log blocks directly into an `AlignedBuf` never copies them on the way, which is what `moonpool-journal` does.

```rust
let mut page = blocks.buffer(1)?;          // aligned for this file
encode_page(page.as_mut_slice());          // built in place
let page = blocks.write_blocks_durable_aligned(7, page).await?; // no copy
```

`BlockFile`'s owned methods are forgiving where the borrowed ones refuse: a buffer that misses the file's memory alignment is copied once into one that meets it, and that one comes back. Only the first request moves the buffer; the rare short transfer is resumed through the borrowed form. `AlignedBuf::truncate` shortens a buffer in place (the last, partial chunk of a fill), and `is_aligned_to` checks an address. The simulator keeps the trait's defaults, the borrowed transfer on the buffer's bytes, so every fault, hint and oracle applies to the owned form unchanged.

`write_at_durable` is a write and a data sync in one call. The Tokio backend issues `pwritev2(RWF_DSYNC)` on Linux and falls back to `pwrite` plus `fdatasync` on a kernel that refuses the flag and on other platforms. The simulator models it as exactly `write_at` then `sync_data`: the write lands dirty, a `hint!` ("storage: durable write landed, not synced") names the window before the sync, and the sync commits it. Sync failures, lying syncs, the barrier-bounded crash model and the lost-synced-write oracle all apply unchanged. The simulated sync is file-wide, which is stronger than the contract: code that relies on a durable write covering an earlier unsynced write passes in simulation and loses data on Linux, so a durable write belongs where its range is the file's only unsynced write.

## Turning Faults On

`StorageConfiguration` holds the timing and every fault probability. All faults are off by default, and a family at zero draws no randomness. The builder picks a profile per seed:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(Replica::new()))
    .enable_chaos([Chaos::Storage(ChaosMode::Swarm), Chaos::BuggifyKnobs])
    // A phantom write makes a replica forget a vote it cast: out of contract.
    .storage_fault_mask(StorageFaultMask::all().without(StorageFault::PhantomWrite))
    .chaos_duration(Duration::from_secs(30))
```

`ChaosMode::Random` uses `StorageConfiguration::random_for_seed()`: fault rates between 0.001% and 0.1%, IOPS from 10K to 100K, bandwidth from 50 to 500 MB/s. `ChaosMode::Swarm` then switches each family off with 50% probability. `Chaos::BuggifyKnobs` spikes a knob on some seeds: IOPS down to 100 to 5,000, sync failures up to 5% to 20%, stalls at 10% to 50%. The `StorageFaultMask` is applied last and draws nothing, so masked and unmasked seeds sample identically.

| `StorageFault` | Knobs | What it finds |
|----------------|-------|---------------|
| `Corruption` | `read_corruption_probability`, `write_corruption_probability` | Missing checksums. Damage is deterministic: a retry returns the same wrong bytes. |
| `Eio` | `read_eio_probability`, `write_eio_probability` | Error paths. An error is not corrupt data, and both halves need driving. |
| `Misdirect` | `misdirect_read_probability`, `misdirect_write_probability` | Records without their own address in the checksum. |
| `PhantomWrite` | `phantom_write_probability` | Firmware that acknowledges a write it never stored. |
| `SyncFailure` | `sync_failure_probability` | Unchecked `fsync` results. A failed `sync_dir` reports `storage_sync_fault` on the timeline. |
| `ShortTransfer` | `short_transfer_probability` | Callers that treat a `read_at` count as "all of it". Never shortened to zero. |
| `DirEntryLoss` | `unsynced_dir_entry_loss_probability` | Engines that sync the file and forget the directory. |
| `CrashDamage` | `crash_lost_probability`, `crash_latent_fault_probability`, `shorn_write_probability` | Recovery. See the crash model below. |
| `Degradation` | `disk_stall_*`, `disk_throttle_*` | Timeout cascades and backpressure collapse. |
| `DiskFailure` | `disk_failure_probability` | A missing timeout around I/O. |
| `DroppedMutationDiscard` | `dropped_mutation_discard_probability` | Nothing by itself. It is the other half of "a dropped write still lands" (below). |

## Timing, Episodes and Dead Disks

Steady state is a SATA SSD by default: 25,000 IOPS, 150 MB/s, reads at 50 to 200 µs, writes at 100 to 500 µs, syncs at 1 to 5 ms. `fast_local()` is 1M IOPS, 1 GB/s and 1 µs everywhere, with no faults. [Calibration](../appendix/07-calibration.md) replaces the defaults with your machine's numbers.

Real disks degrade in **episodes**, which FoundationDB models with its `DiskFailureInjector`. A **stall** freezes a disk for `disk_stall_duration`. A **throttle** divides IOPS and bandwidth by the two multipliers for `disk_throttle_duration`. An episode is keyed by the owning process, not the file, so one garbage-collection pause stalls every file the machine owns at once, and the burst of completions when it lifts is the backpressure spike that overflows real queues.

A **failed disk** never answers. Every read, write, sync or `set_len` after the failure stays `Pending` for the rest of the run, like FoundationDB's `failedDisk`. The stall detector then reports a deadlock, which is the verdict: a process that cannot survive a hung disk has a bug. At most one disk is failed at a time, so a quorum never loses two members this way. A crash or wipe of the owner replaces the disk and fails the parked operations with `OperationInterrupted`. `SimWorld::fail_disk_for_process(ip)` is the scripted form.

What happens to a write when its future is **dropped**? Code wraps a write in a timeout or a `select!` and moves on. On a real backend the write does not stop. Tokio's `spawn_blocking` task keeps running, and on io_uring the kernel owns the operation and the runtime holds its buffer until it completes. So the simulator does the same. A dropped write, positioned write, sync or `set_len` keeps its scheduled completion. The effect lands at that time, with the same fault rolls as an awaited operation, and nobody is woken. Closing a handle with a stream write in flight follows the same rule. A code path that assumes "dropped means not written" fails here, not in production. The `dropped_mutation_discard_probability` coin, drawn once at drop time, models the write that never reached the device: it is discarded whole. A dropped mutation is never half applied. A crash, a wipe or shutdown fails a dropped mutation still in flight like any other in-flight operation, and a mutation parked on a failed disk never lands. A dropped read is cancelled, because it has no effect to land.

A **full disk** refuses growth. `SimWorld::set_disk_capacity(ip, Some(DiskCapacity::new(bytes)))` gives one process's disk a size: the sum of its files' lengths, an unlinked file still open included. A write past the end of a file, or a `set_len` that grows one, fails with `io::ErrorKind::StorageFull` (`ENOSPC`) when the growth does not fit, and none of its bytes land. A write inside a file's length needs no space, so a preallocated file stays writable on a full disk. Deleting or shrinking a file frees space at once. `DiskCapacity::amplified(n)` counts each byte `n` times, so an ordinary workload fills a disk during a short run. The capacity is the harness's choice, not a rate: it draws nothing, holds across a crash or a wipe, and recovery mode lifts every capacity. `SystemSample::space` reports the disk's `DiskSpace { total_bytes, free_bytes }` (`statvfs` in production), so the code under test can see the disk fill before it refuses.

## The Barrier-Bounded Crash Model

A sync is the only barrier a file has. Everything written since the last one is up for grabs.

Every simulated file keeps two images: the **durable** bytes as of the last successful sync, and the **visible** bytes reads observe. A write dirties sectors (512 bytes, `SECTOR_SIZE`) in the visible image. A sync commits them. On a crash each dirty sector resolves **independently**:

| `CrashOutcome` | The sector afterwards |
|----------------|-----------------------|
| `KeptOld` | Its last durable contents. The unsynced write is gone. |
| `KeptNew` | The unsynced write, intact. |
| `Lost` | The fill pattern: never written. |
| `LatentFault` | The new bytes, but reads return deterministic damage. |
| `Shorn` | A sub-sector mix of old and new bytes (opt-in). |

Three knobs shape the whole crash. `clean_crash_probability` (10%, FoundationDB's number) keeps every unsynced write. `correlated_rollback_probability` (25%) rolls back a contiguous run of up to `correlated_rollback_max_run` sectors together, the erase-block damage Zheng measured at FAST'13. `length_survives_crash_probability` (50%) decides whether an unsynced `set_len` holds. This is what makes "the later record landed while the earlier one is partial" reachable, which is the case journal recovery exists for.

A lost sector reads the file's **fill pattern**. `garbage_fill_probability` draws it per file: zeros on some, garbage on others (50/50 under `random_for_seed`). Zeros are the dangerous real case (SATA `RZAT`, NVMe `DLFEAT`, unwritten extents), so recovery that infers "never written" from "reads as zero" has to be driven red on both.

A reboot runs the crash through `SimWorld::simulate_crash_for_process(ip, close_files)`. Every pending operation of that process completes with an interrupted error. `wipe_storage_for_process(ip)` deletes everything the process owns, which is what `RebootKind::CrashAndWipe` does after the crash. Each crash leaves a `FileCrashReport` (`take_storage_crash_reports()`), and every injected fault a `StorageFaultRecord` (`take_storage_fault_records()`).

## Directory Entries Crash Too

```text
create_dir_all("db")   // the directory is visible
sync_dir(".")          // the db name is durable
open("db/wal", create) // a directory entry
write(..)              // the file fills
sync_all(file)         // the bytes are durable
```

After that sequence a crash may still leave no `db/wal`. The bytes were durable, the name was not. SQLite, PostgreSQL and LMDB follow every create, delete and rename with a directory sync, and `sync_dir` is that call. The simulator keeps a visible namespace and a durable one. Each divergence resolves independently at crash time under `unsynced_dir_entry_loss_probability`: a created name may be missing, a deleted one may be back. `list_dir` shows the visible namespace, which is what `readdir` sees.

## The Lost-Synced-Write Oracle

At every sync, each committed sector is stamped with a CRC of the bytes the caller was told are durable. That is FoundationDB's `AsyncFileWriteChecker`. After a crash, a stamped sector that no longer matches is a **simulator bug**, and the run fails with an assertion.

Unless the barrier-violation family is armed. With `barrier_violation_probability > 0`, a sync occasionally **lies** about a sector, and the oracle flips to must-detect mode: the loss is recorded as an expected `StorageFaultKind::LostSyncedWrite`. This family is how a consumer proves that cluster-level recovery heals a single lying disk, the fsyncgate class of bug. `SimWorld::corrupt_durable_out_of_band(path, sector)` plants a deliberate violation to test the oracle itself.

The stamp covers the synced prefix of a file's last partial sector. Growing the file makes the rest of the sector addressable and unsynced, and leaves the stamped prefix alone. A shrink inside a sector narrows the stamp to the bytes that survived.

## Per-Process Disks

Every file is owned by a process IP, and faults, timing and episodes resolve against the owner's profile. One node can have a flaky SSD while the rest are healthy:

```rust
let degraded = StorageConfiguration {
    read_corruption_probability: 0.01,
    ..StorageConfiguration::default()
};
sim.set_process_storage_config("10.0.1.2".parse()?, degraded);
```

Random faults are **layout-blind**: they hit whatever sector an operation touches, and most of a preallocated file is zeros nobody reads. [Aiming Storage Faults](33-aimed-faults.md) shows how a format moves the damage onto the bytes recovery has to judge, and how a replicated system keeps one clean copy of every record by construction.
