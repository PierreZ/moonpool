# Storage Faults

<!-- toc -->

## Disks Lie

Every database developer eventually learns this lesson. `write()` returns success, but the data never reaches the platter. `fsync()` completes, but the drive's firmware lied about flushing its cache. A cosmic ray flips a bit in DRAM between computing a checksum and writing to disk. A firmware bug directs a write to the wrong sector.

These are not hypothetical failures. TigerBeetle's documentation catalogs them with references to real incidents: LSE studies showing 8.5% of SATA drives developing silent corruption, firmware bugs causing misdirected writes across drives in a RAID array, and enterprise SSDs that acknowledge fsync without actually flushing.

Moonpool's storage fault injection is modeled on TigerBeetle's fault taxonomy. The goal is to test that your data integrity code actually works, not by hoping these faults happen in production, but by making them happen deterministically in simulation.

## The Fault Taxonomy

Moonpool's `StorageConfiguration` controls the fault families below. All are
off by default, and a family whose probability is zero draws no randomness at
all, so a run with faults disabled stays byte-for-byte deterministic.

### Read Corruption

A read operation returns wrong data. The file contains correct bytes, but the value returned to the application has been corrupted. This models ECC failures, DRAM bit flips, and controller firmware bugs.

**What it tests:** Checksum validation on reads. If your system trusts data without verifying checksums, read corruption will silently propagate bad data through the system.

### Write Corruption

A write operation stores wrong data. The application writes correct bytes, but what lands on disk is different. This models controller bugs, bad sectors, and write buffer corruption.

**What it tests:** Read-after-write verification and end-to-end checksums. Systems that compute checksums before writing and verify after reading will detect write corruption. Systems that do not will store garbage.

### I/O Errors

A read or a write fails outright: the device reports that it could not serve
the request. This is an *operating condition*, and it is a different thing
from a read that succeeds and returns corrupt bytes — `read_eio_probability`
and `write_eio_probability` are separate families from the two above for
exactly that reason.

**What it tests:** Error paths. Code that treats an error as "corrupt data" —
or worse, ignores the result — is wrong on real disks, and both halves have to
be driven.

### Crash Faults (Torn Writes)

The system crashes mid-write. Some sectors are written, others are not. This models power failures, kernel panics, and OOM kills during I/O.

**What it tests:** Write-ahead logging, atomic write protocols, and crash recovery. Any system that performs multi-step writes without a journal or atomic commit is vulnerable to torn writes. See *The Barrier-Bounded Crash Model* below for the shapes a crash can leave behind.

### Short Transfers

A read or write moves fewer bytes than asked for and returns the count. This is ordinary POSIX behaviour that callers routinely forget.

**What it tests:** Whether a caller loops. Code that treats a `read_at` return value as "all of it" silently drops data, which `short_transfer_probability` makes happen on demand.

### Lost Directory Entries

A create, delete, or rename that was never followed by a directory sync does not survive a crash — however thoroughly the file's contents were synced.

**What it tests:** Whether an engine syncs the directory holding its files, not just the files. See *File Durability Is Not Directory Durability* below.

### Misdirected Writes

A write lands at the wrong location. The application writes to offset A, but the data ends up at offset B. This models firmware bugs and controller errors that TigerBeetle specifically documents as real-world failures.

**What it tests:** Per-record addressing verification. Systems that embed the expected offset in each record's header can detect misdirected writes. Systems that trust the filesystem to put data where it was told will read the wrong records.

### Misdirected Reads

A read returns data from the wrong location. The application reads offset A, but gets the contents of offset B. Same root causes as misdirected writes, from the read side.

**What it tests:** Same as misdirected writes. Checksums that include the expected position catch this.

### Phantom Writes

A write appears to succeed but does not persist. The `write()` call returns `Ok(n)` and even `fsync()` completes, but the data is gone after a restart. This models drive firmware that lies about durability.

**What it tests:** Durability verification after recovery. Systems that write, sync, crash, and restart must verify that their data survived. Phantom writes ensure this verification logic works.

### Sync Failures

`sync_all()` returns an error. This models disk errors during flush, full disks, and I/O errors that only manifest at sync time.

**What it tests:** Error handling in durability-critical code paths. Many systems call `fsync()` but do not check the return value. In simulation, a sync failure is a loud signal that your error handling has a gap.

## Performance Simulation

Beyond faults, moonpool simulates realistic storage performance characteristics:

| Parameter | Default | Description |
|-----------|---------|-------------|
| IOPS | 25,000 | Operations per second (SATA SSD range) |
| Bandwidth | 150 MB/s | Maximum throughput |
| Read latency | 50-200us | Per-operation delay |
| Write latency | 100-500us | Per-operation delay |
| Sync latency | 1-5ms | Per-sync delay |

These parameters ensure that storage-heavy code paths experience realistic timing, which is important for testing timeout logic and concurrent I/O patterns.

## Dynamic Disk Degradation Episodes

Steady-state timing is a lie of a different kind. Real disks do not degrade at a fixed rate. They degrade **episodically**: a garbage-collection pause freezes I/O for 100-500ms, a thermal event throttles throughput for seconds, firmware stalls under load. These episodes are what trigger the interesting failures: timeout cascades, backpressure collapse, and recovery bottlenecks that a constant 150 MB/s never produces. FoundationDB models this with its `DiskFailureInjector`, and moonpool borrows the idea.

Two episode kinds sit on top of the steady-state formula, both off by default and scoped **per process** (per owning IP):

| Episode | Config | While active |
|---------|--------|--------------|
| Stall | `disk_stall_probability` / `disk_stall_duration` | The disk is frozen until the window expires. Any I/O scheduled during the stall waits out the remaining time, then takes its normal latency. |
| Throttle | `disk_throttle_probability` / `disk_throttle_duration` | Effective IOPS and bandwidth are divided by `disk_throttle_iops_multiplier` and `disk_throttle_bandwidth_multiplier`. |

Before each read, write, or sync, the owning process's episode state machine runs: an expired episode clears, an active one stays, and an idle disk rolls the dice to enter a new episode. The episode is keyed by the process IP, not the file, because real degradation is a property of the physical disk: a garbage-collection pause or a firmware stall hits **every file the machine owns at once**. So one episode freezes all of a process's open files together, and a second machine in the same simulation keeps running at full speed. That correlated freeze, where a whole machine's I/O completes at the same moment the window lifts, is exactly the backpressure spike that overflows queues in real systems.

```rust
// A disk that stalls on every operation for 50ms
let stalling = StorageConfiguration {
    disk_stall_probability: 1.0,
    disk_stall_duration: Duration::from_millis(50),
    ..StorageConfiguration::fast_local()
};
```

The key property is that **a disabled disk never touches the random number stream**. When both probabilities are zero, the state machine returns before drawing any randomness, so steady-state runs stay byte-for-byte deterministic. Chaos runs enable low-rate episodes through `random_for_seed()`, swarm masking, and buggify knob spikes, the same machinery every other storage fault family uses.

## Disk Failure: I/O That Never Completes

A stall is a disk that answers late. A **failed disk** is a disk that never answers. Every read, write, sync, or `set_len` issued to it after the failure is accepted and stays `Pending` for the rest of the run. Nothing returns an error, nothing times out on the disk's behalf, and the world's event queue is empty while the caller waits. FoundationDB models this with `failedDisk`, where `waitUntilDiskReady()` returns `Never()`; moonpool does the same through `disk_failure_probability`, a per-operation coin that is off by default.

This is the storage fault that finds a missing timeout. Code that awaits an `fsync` inline, with no deadline and no way for the rest of the process to notice, hangs. The runner's stall detector then sees a world with no events and no progress, requests a graceful shutdown, and reports a **deadlock**. That verdict is the point: a disk that stops answering is something a real machine does, and a process that cannot survive it has a bug.

```rust
// A disk that fails on its first operation
let failing = StorageConfiguration {
    disk_failure_probability: 1.0,
    ..StorageConfiguration::fast_local()
};
```

The failure is keyed by the owning process, like an episode: one failure hangs every file the machine owns. Two rules keep it a fault a system can be expected to survive rather than a way to make a run unwinnable:

- **At most one disk is failed at a time.** The coin is not drawn while another disk is failed, so a quorum system never loses more than one member to a hung disk. This is the family's floor.
- **A crash or wipe of the owning process replaces the disk.** The operations it parked fail with `OperationInterrupted` along with the rest of the process's in-flight I/O, the failure is cleared, and the budget is free for the next disk to draw the coin. Until then the failure has no expiry: recovery mode stops new failures but keeps one already in force, exactly as it keeps a killed process.

A scripted fault injector can fail a disk directly with `SimWorld::fail_disk_for_process(ip)`. That path consumes no randomness and ignores the one-at-a-time budget, because the injector owns its own budget. A parked operation samples no latency and enters no episode, so a hung I/O never moves the seed's random stream either.

## Exact Asynchronous Operations

Read, write, sync, and set-length calls schedule work and return
`Poll::Pending`. Each submission receives a unique `OperationId`. Its
`StorageEvent` carries that exact ID, the submitting handle, and the expected
operation kind. `StorageEngine` keeps an explicit pending entry and later an
explicit `Result` for the same ID. A missing entry is an invalid operation, not
implicit success.

Network establishment is asynchronous too. Bind, connect, and accept also need
the scheduler to advance. Established stream writes can accept bytes into their
send buffer immediately, and reads can complete immediately when bytes are
already buffered. Do not use the old rule that all network operations are ready
while all storage operations are pending.

`SimulationBuilder::run()` already interleaves the deterministic executor and
the scheduler for normal process and workload tests. If you write a low-level
provider test, drive its future and `SimWorld` together on Moonpool's executor:

```rust,ignore
async fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    futures::pin_mut!(future);
    futures::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(output) => Poll::Ready(output),
        Poll::Pending if sim.has_pending_events() => {
            sim.step();
            cx.waker().wake_by_ref();
            Poll::Pending
        }
        Poll::Pending => Poll::Pending,
    })
    .await
}

let mut executor = moonpool_sim::executor::Executor::new(seed);
executor.block_on(async move {
    let provider = sim.storage_provider(ip);
    drive(&mut sim, async move {
        let mut file = provider.open("test.txt", OpenOptions::create_write()).await?;
        file.write_all(b"hello").await?;
        file.sync_all().await
    })
    .await
})?;
```

The helper polls the provider future, steps one scheduled event when necessary,
and lets the event's waker make the future runnable again. If the future is
pending with no scheduled path to progress, the executor reports a deadlock
with the seed.

## StorageEngine Ownership

`StorageEngine` owns the whole simulated disk surface: persistent file data,
path lookup, open handles, default and per-process configurations, disk
episodes, pending operations, completed results, fault decisions, and storage
wakers. `SimWorld` only schedules the engine's requested events and
cancellations, records returned faults, and invokes the returned wake batch
after releasing the world lock.

That split keeps operation ordering explicit. Completion events never search
for the oldest operation with the same file and kind. Concurrent operations on
one file can finish in scheduler order and wake only their own callers. Dropping
a future cancels its schedule and removes its pending result state.

## Independent Open Handles

Persistent contents belong to a file record. Cursor position, access options,
closed state, and pending-operation IDs belong to an open handle. Opening the
same path twice therefore creates two handles over one file:

- Seeking or reading through one handle does not move the other handle's cursor
- Read-only and write-only permissions are enforced per handle
- Dropping or closing one handle cancels its pending work without deleting the
  persistent file or closing sibling handles
- Append handles resolve the current end of the shared file when scheduling a
  write

This matches the behavior applications expect from real file descriptors and
makes concurrent-handle races meaningful rather than accidentally sharing one
cursor.

## Per-Process Storage Configuration

Storage fault injection is scoped per process. Each process is identified by its IP address, and you can assign different `StorageConfiguration` to different processes. This models real-world heterogeneous hardware: one node with a flaky SSD, another with a healthy disk.

The engine maintains a global configuration as the default plus optional
per-process overrides. For each file operation it resolves the profile by the
file owner's IP, then updates that process's disk-degradation episode before
calculating latency and faults.

Set per-process configuration through `SimWorld`:

```rust
// Give process 10.0.1.2 a degraded disk
let degraded = StorageConfiguration {
    read_corruption_probability: 0.01,  // 1% read corruption
    write_corruption_probability: 0.005,
    ..StorageConfiguration::default()
};
let degraded_ip = "10.0.1.2".parse().expect("valid process IP");
sim.set_process_storage_config(degraded_ip, degraded);
```

Every persistent file is tagged with its owning process IP. Fault injection
decisions such as corruption, latency, and sync failure use that owner rather
than a single global profile.

## Crash and Wipe Operations

Two `SimWorld` methods handle storage lifecycle during process failures:

**`simulate_crash_for_process(ip, close_files)`** applies crash behavior to the
process's persistent files, including torn-write fault injection. Every pending
read, write, sync, or set-length operation for those files completes with an
interrupted error and wakes its exact waiter. When `close_files` is true, the
affected handles are also marked closed.

**`wipe_storage_for_process(ip)`** deletes all persistent storage owned by the
given process and invalidates its handles. This models total disk failure or
replacing a machine. The `CrashAndWipe` reboot kind calls both: crash first,
then wipe. The wipe happens immediately.

Global simulation shutdown follows the same explicit-result rule. It cancels
every pending storage schedule, records a shutdown error for each operation,
marks handles closed, and returns all waiters for wakeup. No task is left parked
behind a synthetic timer, and no crash-cleared operation can report `Ok(())`.

## Configuration in Practice

For chaos testing, use `StorageConfiguration::random_for_seed()`. This randomizes both performance parameters and fault probabilities based on the simulation seed:

```rust
let storage_config = StorageConfiguration::random_for_seed();
// Fault probabilities: 0.001% to 0.1% (low but present)
// IOPS: 10K to 100K
// Bandwidth: 50-500 MB/s
```

For fast unit tests, use `StorageConfiguration::fast_local()`:

```rust
let storage_config = StorageConfiguration::fast_local();
// 1M IOPS, 1 GB/s, 1us latencies, zero faults
```

The fault probabilities in `random_for_seed()` are intentionally low (0.001% to 0.1%). Storage faults at higher rates would prevent the system from making progress. The goal is a steady trickle of faults that occasionally exercises corruption detection and recovery, not a deluge that makes every I/O fail.

## Positioned I/O, Direct I/O, and Alignment

A journal or a pager does not want a seek cursor. It wants to read page 7 and
write page 12, possibly at the same time, from several tasks, without any of
them disturbing the others' idea of "where I am".

`StorageFile::read_at(offset, buf)` and `write_at(offset, buf)` are that
primitive. They take `&self`, address the offset literally, and never touch
the stream cursor, so non-overlapping ranges of one file can be read and
written concurrently. They are `read` and `write`, not `read_exact` and
`write_all`: both return the number of bytes moved, and a read returns 0 at
end of file. `short_transfer_probability` makes the simulator move a non-empty
prefix instead of the whole buffer, deterministically, so a caller that
assumes a full transfer can be driven red.

Everything a database needs from an open is on `OpenOptions`, not on a second
provider:

```rust
let file = provider
    .open(
        "db/pages",
        OpenOptions::read_write().direct_io(DirectIo::Required),
    )
    .await?;
```

- `DirectIo::Disabled` is ordinary buffered I/O.
- `DirectIo::Optional` asks for uncached I/O and accepts a documented fallback
  where the filesystem refuses it; `file.is_direct_io()` says which one you
  got.
- `DirectIo::Required` fails the open rather than downgrading silently, and
  opens a file that already exists — it never creates one.

That last rule is a deliberate boundary, and the native open shows why. A
refused `O_CREAT | O_DIRECT` still leaves the file behind: the file is created
during path resolution and `O_DIRECT` is rejected afterwards. Cleaning that up
is a namespace protocol — stage an inode elsewhere, publish the name — not a
file open, and it is not the provider's to run on your behalf. Whoever owns the file's format
owns that protocol, because only they know whether the name has to be durable,
what a half-created file means, and how recovery finds one. So a journal
bootstraps its own file, in the order its recovery expects:

```rust
// 1. create it with an ordinary open, 2. make the file and its name durable
let created = provider.open("db/wal", OpenOptions::create_new_write()).await?;
created.sync_all().await?;
drop(created);
provider.sync_dir("db").await?;

// 3. now require the capability of the file that exists, 4. format and recover
let wal = provider
    .open("db/wal", OpenOptions::read_write().direct_io(DirectIo::Required))
    .await?;
```

A `Required` open of an existing file *does* honour `truncate`, and applies it
only after direct I/O is secured, so a refused capability check cannot have
modified the file.

Direct I/O is **not durability**. An uncached write is still only visible
until a `sync_all()` / `sync_data()` makes it durable, exactly like a buffered
one. What it does buy is that a read comes from the device rather than from a
page the kernel may have marked clean after a failed flush (Rebello, ATC'20).

`file.constraints()` reports what the open demands, as three independent
numbers — offset alignment, length alignment, and memory alignment — because a
device may constrain them differently. None of them is your block or page
size, and none is a crash-atomicity unit. Every transfer is checked against
them, so a misaligned direct-I/O call fails with `InvalidInput` in simulation
exactly as it earns `EINVAL` from a kernel. `AlignedBuf` is the one
aligned-buffer facility in moonpool; layers above it reuse it rather than
growing their own.

The numbers are discovered, not assumed: production asks the kernel
(`statx(STATX_DIOALIGN)`) for the device's real requirement, falls back to the
page size as a documented bound where the kernel will not say, and refuses to
offer direct I/O at all where it can do neither — advertising an alignment
that turns out to be too weak would hand callers a buffer the device rejects.
The simulation reports the disk geometry it was configured with.

**The stream API and alignment are mutually exclusive.** A file with I/O
constraints refuses `AsyncRead` / `AsyncWrite` with `ErrorKind::Unsupported`,
in both backends, because a shared cursor cannot be kept aligned: a stream
transfer starts wherever the cursor happens to be, and a short one leaves it
somewhere no subsequent request may start from. Seeking still works — it moves
a cursor without transferring anything — and `read_at` / `write_at` are what
such a file offers instead.

## File Durability Is Not Directory Durability

```text
open("db/wal", create)   // creates a directory entry
write(..)                // fills the file
sync_all(file)           // the bytes are durable
```

After that sequence a crash may leave no `db/wal` at all. The bytes were made
durable; the *name* that reaches them was not. Engines that get this right
(SQLite, PostgreSQL, LMDB) follow every create, delete, and rename with a
directory sync, and `StorageProvider::sync_dir(path)` is that call. It lives
on the provider, beside `rename` and `delete`, because it concerns the
filesystem namespace rather than the contents of an open file.

The simulator keeps two namespaces: the visible one, which a create, delete,
or rename changes immediately, and the durable one, which only `sync_dir`
promotes into. On a crash each divergence between them resolves independently
under `unsynced_dir_entry_loss_probability`: a created name may not be there,
a deleted one may be back. The family is off by default and draws no
randomness while off, so a crash keeps the namespace it had unless a test asks
otherwise.

## The Barrier-Bounded Crash Model

A sync is the only barrier a file has, and everything written since the last
one is up for grabs.

Every simulated file keeps two images: the **durable** bytes as of its last
successful sync, and the **visible** bytes reads observe. A write dirties
sectors in the visible image; a sync commits them. On a crash each dirty
sector resolves **independently**:

| Outcome | What the sector holds afterwards |
|---------|----------------------------------|
| `KeptOld` | Its last durable contents — the unsynced write is gone. |
| `KeptNew` | The unsynced write, intact. |
| `Lost` | The file's fill pattern: it reverts to never-written. |
| `LatentFault` | The new bytes, but reads return deterministic damage. |
| `Shorn` | A sub-sector mix of old and new bytes (opt-in). |

An occasional fully clean crash (`clean_crash_probability`, 10% — FDB's
number) and an occasional correlated rollback of a contiguous run
(erase-block damage, Zheng FAST'13) round out the shapes, and an unsynced
length change resolves the same way. This is what makes "the later record
landed while the earlier one is partial" — the case journal recovery exists to
survive — actually reachable.

Two properties are worth stating plainly, because code hides behind their
opposites:

- **A lost sector reads the fill pattern, which is zeros on some files and
  garbage on others** (`garbage_fill_probability`, drawn per file). Zeros are
  the dangerous real case (SATA `RZAT`, `NVMe` `DLFEAT`, unwritten extents),
  so recovery code that infers "never written" from "reads as zero" has to be
  driven red on both.
- **Damage is deterministic.** A corrupted sector returns the same wrong bytes
  on every read; a retry never heals it.

### The Lost-Synced-Write Oracle

At every sync, each committed sector is stamped with a CRC of the content the
caller was told is durable (FoundationDB's `AsyncFileWriteChecker` pattern).
After a crash, a stamped sector that no longer matches is a **simulator bug**
and fails the run loudly — unless the opt-in barrier-violation family is armed
(`barrier_violation_probability > 0`), in which case a sync occasionally
*lies* about a sector and the oracle flips to must-detect mode, reporting the
loss as an expected `LostSyncedWrite`. That family is how a consumer proves
cluster-level recovery heals a single lying disk (the fsyncgate class).

### Aiming Faults

Random fault families are gated by a caller-provided eligibility mask,
`(path, sector) -> bool`, so a replication-aware harness can enforce "never
damage all copies of one record" without moonpool knowing what a replica is
(TigerBeetle's `ClusterFaultAtlas` pattern). Rolls happen *before* the mask is
consulted, so installing one never shifts the random stream.

Directed tests reach for the targeted API on `SimWorld` instead:
`corrupt_file(path, sectors)`, `fail_file_with_eio(path, sectors, target)`,
`clear_file_eio`, and — to test the oracle itself —
`corrupt_durable_out_of_band`. What each crash did is available from
`take_storage_crash_reports()`, and every fault injected from
`take_storage_fault_records()`.

Fault coordinates are **file plus flat sector offset**. There is no region and
no sub-file namespace: what the bytes at an offset mean belongs to the format
written on top.

## Blocks Are a View, Not a Device

Storage engines address data in fixed-size blocks. `BlockFile<F>` is that
arithmetic, and only that arithmetic:

```rust
let file = provider.open(path, options).await?;
let blocks = BlockFile::new(file, 4096)?;

blocks.grow_to_blocks(1024).await?;
let mut page = blocks.buffer(1);     // aligned for this file
blocks.write_blocks(7, page.as_slice()).await?;
blocks.sync().await?;
```

`read_blocks` fills its buffer completely or fails `UnexpectedEof`, and
`write_blocks` writes every byte or fails — looping over the partial transfers
the file below is allowed to return is the job this layer exists to do.

What it deliberately is not:

- It **wraps one already-open file**. It takes no path, stores no path, and
  cannot open, create, rename, or delete anything. There is no
  `BlockFile::open` and no `BlockProvider`: an operation that needs a path is
  an operation for `StorageProvider`.
- It is not a directory, several files, a region table, a manifest, a
  namespace, or a virtual disk. A database's logical zones — journal,
  superblock, pages — are byte offsets in one file. The file API does not grow
  a `superblock-file` because the format has a superblock.
- The block size is the *caller's* unit. It must be a multiple of the file's
  I/O alignment, and it is neither that alignment nor a crash-atomicity unit:
  a two-block write tears across a crash like any other.
- It is not durability. `write_blocks` returns when bytes are visible; `sync`
  is what makes them durable.

Because it is generic over `StorageFile`, there is one block layer over two
backends — the simulated file and the real one — rather than a production
block device and a separate simulated one.
