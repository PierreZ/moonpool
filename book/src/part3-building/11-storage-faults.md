# Storage Faults

<!-- toc -->

## Disks Lie

Every database developer eventually learns this lesson. `write()` returns success, but the data never reaches the platter. `fsync()` completes, but the drive's firmware lied about flushing its cache. A cosmic ray flips a bit in DRAM between computing a checksum and writing to disk. A firmware bug directs a write to the wrong sector.

These are not hypothetical failures. TigerBeetle's documentation catalogs them with references to real incidents: LSE studies showing 8.5% of SATA drives developing silent corruption, firmware bugs causing misdirected writes across drives in a RAID array, and enterprise SSDs that acknowledge fsync without actually flushing.

Moonpool's storage fault injection is modeled on TigerBeetle's fault taxonomy. The goal is to test that your data integrity code actually works, not by hoping these faults happen in production, but by making them happen deterministically in simulation.

## The Fault Taxonomy

Moonpool's `StorageConfiguration` controls seven types of storage faults:

### Read Corruption

A read operation returns wrong data. The file contains correct bytes, but the value returned to the application has been corrupted. This models ECC failures, DRAM bit flips, and controller firmware bugs.

**What it tests:** Checksum validation on reads. If your system trusts data without verifying checksums, read corruption will silently propagate bad data through the system.

### Write Corruption

A write operation stores wrong data. The application writes correct bytes, but what lands on disk is different. This models controller bugs, bad sectors, and write buffer corruption.

**What it tests:** Read-after-write verification and end-to-end checksums. Systems that compute checksums before writing and verify after reading will detect write corruption. Systems that do not will store garbage.

### Crash Faults (Torn Writes)

The system crashes mid-write. Some bytes are written, others are not. This models power failures, kernel panics, and OOM kills during I/O.

**What it tests:** Write-ahead logging, atomic write protocols, and crash recovery. Any system that performs multi-step writes without a journal or atomic commit is vulnerable to torn writes.

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
    read_fault_probability: 0.01,  // 1% read corruption
    write_fault_probability: 0.005,
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

## The BlockDevice Contract

The file API above is deliberately POSIX-flavored, which puts it at the wrong
altitude for a storage *engine*: a WAL, an LSM, or a B-tree pager must
carefully avoid relying on stream semantics (seek, append, auto-extend) while
getting no guarantee it actually needs (atomicity unit, alignment, reorder
window). For that, moonpool-core provides a second, narrower surface:
`BlockDevice` — sector-aligned reads and writes inside named regions, one
explicit durability barrier, and grow-only resize.

```rust,ignore
use moonpool_core::{BlockDevice, BlockDeviceProvider, RegionId, RegionSpec};
use moonpool_sim::{BlockFaultConfig, SimBlockDeviceProvider, SimBlockStore};

let store = SimBlockStore::new(seed, BlockFaultConfig::default());
let provider = SimBlockDeviceProvider::new(store.clone());

let device = provider
    .create("db", &[
        RegionSpec { name: "wal", size: 1024 * 4096 },
        RegionSpec { name: "superblock", size: 4096 },
    ])
    .await?;
device.write(RegionId(0), 0, &entry).await?;   // visible, not durable
device.persist().await?;                        // durability barrier
```

The contract clauses documented on the trait ARE the feature:

- **Atomicity unit is one sector (4096 bytes), nothing larger.** A crash may
  independently leave each sector of a multi-sector write old, new, or
  unreadable.
- **Writes between two `persist()` calls may reach disk in any order.** Only
  the barrier orders writes.
- **Completion of `write()` implies visibility, not durability.**
- **Never-written sectors read unspecified bytes** — zeros, stale data, or
  garbage. Never infer written-ness from content.
- **EIO is an operating condition**, distinct from a successful read of
  corrupt bytes; corrupt reads are deterministic and retries never heal.
- `create()` is atomic: the device is invisible to `open()` until its first
  `persist()`.

### Barrier-Bounded Crash Model

`SimBlockStore::crash_device()` resolves every sector written since the last
successful `persist()` **independently**: kept old, kept new, lost (reverts to
the fill pattern — zeros or garbage, chosen per seed), or left with a latent
read fault. An occasional fully-clean crash (10%, FDB's number) and an
occasional correlated rollback of a contiguous sector run (erase-block damage)
round out the shapes. This is what makes "persist-record landed while its
entry is partial" — the case CTRL-style journal recovery exists to survive —
actually reachable in simulation.

Fault families (EIO on read/write, read-time corruption, misdirected writes
contained within a region, phantom writes, persist failures) are gated by both
the `BlockFaultConfig` probabilities (per-seed swarm via
`BlockFaultConfig::swarm()`) and a caller-provided eligibility mask
`(path, region, sector) -> bool`, so a replication-aware harness can enforce
"never fault all copies of one record" without moonpool knowing what a replica
is. Directed red tests use the targeted API: `corrupt()`, `fail_with_eio()`,
`wipe_device()`.

### The Lost-Synced-Write Oracle

At every `persist()`, each synced sector is stamped with a CRC of the content
the caller was told is durable. After a crash, a stamped sector that no longer
matches is a **simulator bug** and fails loudly — unless the opt-in
barrier-violation family is armed (`barrier_violation_probability > 0`), in
which case `persist()` occasionally *lies* about a sector and the oracle flips
to must-detect mode, reporting the loss as an expected `LostSyncedWrite` fault
event. Downstream consumers use that family to prove cluster-level recovery
heals a single lying disk (the fsyncgate class of failures).
