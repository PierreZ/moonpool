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

## The Step Loop Pattern

Storage operations in moonpool differ from network operations in one critical way: **storage operations return `Poll::Pending` and require simulation stepping**. Network operations buffer data and return `Poll::Ready` immediately. Storage operations need the simulation engine to advance time and process the I/O.

This means you cannot just `await` a storage operation in a test. You need the step loop pattern:

```rust
let handle = tokio::task::spawn_local(async move {
    // This runs inside the simulation
    let mut file = provider.open("test.txt", OpenOptions::create_write()).await?;
    file.write_all(b"hello").await?;
    file.sync_all().await
});

// Drive the simulation until the task completes
while !handle.is_finished() {
    while sim.pending_event_count() > 0 {
        sim.step();  // Process one simulation event
    }
    tokio::task::yield_now().await;  // Let the spawned task make progress
}

handle.await.unwrap().unwrap();
```

The outer loop checks if the spawned task has finished. The inner loop processes all pending simulation events (which include storage I/O completions). The `yield_now()` gives the spawned task a chance to run after events have been processed.

This pattern is mechanical but important. Without it, storage operations will hang forever waiting for simulation events that never get processed.

## Per-Process Storage Configuration

Storage fault injection is scoped per process. Each process is identified by its IP address, and you can assign different `StorageConfiguration` to different processes. This models real-world heterogeneous hardware: one node with a flaky SSD, another with a healthy disk.

The `StorageState` maintains a global configuration as the default, plus optional per-process overrides in `per_process_configs: HashMap<IpAddr, StorageConfiguration>`. When the simulation needs a config for a file operation, `StorageState::config_for(ip)` checks for a per-process override first, falling back to the global config.

Set per-process configuration through `SimWorld`:

```rust
// Give process 10.0.1.2 a degraded disk
let degraded = StorageConfiguration {
    read_fault_probability: 0.01,  // 1% read corruption
    write_fault_probability: 0.005,
    ..StorageConfiguration::default()
};
sim.set_process_storage_config("10.0.1.2".parse().unwrap(), degraded);
```

Every file opened by a process is tagged with that process's IP (`StorageFileState::owner_ip`). Fault injection decisions (corruption probabilities, latency ranges, sync failures) use the config resolved for the file's owner, not a single global setting.

## Crash and Wipe Operations

Two `SimWorld` methods handle storage lifecycle during process failures:

**`simulate_crash_for_process(ip, close_files)`** simulates a power loss for a specific process. Pending writes are subject to crash fault injection (torn writes), and open file handles are optionally closed. This replaces the old `simulate_crash()` which operated globally.

**`wipe_storage_for_process(ip)`** deletes all persistent storage owned by the given process. This models total disk failure or replacing a machine. The `CrashAndWipe` reboot kind calls both: crash first, then wipe. The wipe happens immediately (not deferred).

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
