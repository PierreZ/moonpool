# TigerBeetle Storage Simulation Analysis

This document analyzes how TigerBeetle simulates storage I/O for deterministic testing, including fault injection, storage timing, and corruption simulation.

## 1. Reference Files

| File | Description |
|------|-------------|
| [`storage.zig`](../../references/tigerbeetle/storage.zig) | Main storage interface with real I/O |
| [`testing-storage.zig`](../../references/tigerbeetle/testing-storage.zig) | Simulated in-memory storage with fault injection |
| [`storage_checker.zig`](../../references/tigerbeetle/storage_checker.zig) | Cross-replica storage validation |
| [`storage_fuzz.zig`](../../references/tigerbeetle/storage_fuzz.zig) | Storage fuzz testing harness |

## 2. Architecture Overview

TigerBeetle's storage simulation provides:

1. **In-memory storage** with deterministic fault injection
2. **Per-zone fault tolerance model** (superblock, WAL, grid)
3. **ClusterFaultAtlas** for coordinated multi-replica fault distribution
4. **Latency simulation** with exponential distribution

The key insight is that storage simulation maintains **pristine data** separately from **faulted state**, allowing faults to be toggled without data loss.

```
┌─────────────────────────────────────────────────────────┐
│                    Testing Storage                       │
├─────────────────────────────────────────────────────────┤
│  memory[]           │ Pristine data as-written          │
│  memory_written     │ Bitset: sectors ever written      │
│  faults             │ Bitset: sectors marked faulty     │
│  overlays           │ Misdirected write tracking        │
├─────────────────────────────────────────────────────────┤
│  reads queue        │ Priority queue by ready_at        │
│  writes queue       │ Priority queue by ready_at        │
└─────────────────────────────────────────────────────────┘
```

## 3. Main Storage Interface

The production `StorageType` (`storage.zig`) provides the interface that both real and simulated storage implement.

### 3.1 Read Struct (lines 22-77)

```zig
pub const Read = struct {
    completion: IO.Completion,
    callback: *const fn (read: *Storage.Read) void,
    buffer: []u8,
    offset: u64,
    target_max: u64,  // For LSE binary subdivision
    zone: vsr.Zone,
    start: ?stdx.Instant,
```

The `target_max` field enables **Latent Sector Error (LSE) recovery** through binary subdivision.

### 3.2 Write Struct (lines 79-87)

```zig
pub const Write = struct {
    completion: IO.Completion,
    callback: *const fn (write: *Storage.Write) void,
    buffer: []const u8,
    offset: u64,
    zone: vsr.Zone,
    start: ?stdx.Instant,
};
```

### 3.3 read_sectors Interface (lines 215-239)

```zig
pub fn read_sectors(
    self: *Storage,
    callback: *const fn (read: *Storage.Read) void,
    read: *Storage.Read,
    buffer: []u8,
    zone: vsr.Zone,
    offset_in_zone: u64,
) void
```

### 3.4 write_sectors Interface (lines 386-410)

```zig
pub fn write_sectors(
    self: *Storage,
    callback: *const fn (write: *Storage.Write) void,
    write: *Storage.Write,
    buffer: []const u8,
    zone: vsr.Zone,
    offset_in_zone: u64,
) void
```

### 3.5 LSE Binary Subdivision Recovery (lines 279-384)

When a read encounters an `InputOutput` error (LSE), the storage layer:

1. **Binary subdivision**: Divides the read into halves recursively
2. **Sector isolation**: Finds the exact failing sector(s)
3. **Zero filling**: Zeros sectors that cannot be read
4. **Checksum treatment**: Treats zeroed sectors as checksum failures

```zig
error.InputOutput => {
    const target = read.target();
    if (target.len > constants.sector_size) {
        // Subdivide: binary search for faulty sector
        read.target_max = (@divFloor(target_sectors - 1, 2) + 1) * constants.sector_size;
        self.start_read(read, 0);  // Retry with smaller window
    } else {
        // At sector granularity: zero and continue
        @memset(target, 0);
        self.start_read(read, target.len);
    }
}
```

## 4. Testing Storage Implementation

The simulated storage (`testing-storage.zig`) provides in-memory storage with fault injection.

### 4.1 Options Struct (lines 51-90)

```zig
pub const Options = struct {
    size: u64,
    seed: u64 = 0,
    replica_index: ?u8 = null,

    // Latency configuration
    read_latency_min: Duration = .{ .ns = 0 },
    read_latency_mean: Duration = .{ .ns = 0 },
    write_latency_min: Duration = .{ .ns = 0 },
    write_latency_mean: Duration = .{ .ns = 0 },

    // Fault probabilities
    read_fault_probability: Ratio = Ratio.zero(),
    write_fault_probability: Ratio = Ratio.zero(),
    write_misdirect_probability: Ratio = Ratio.zero(),
    crash_fault_probability: Ratio = Ratio.zero(),

    fault_atlas: ?*const ClusterFaultAtlas = null,
    grid_checker: ?*GridChecker = null,
};
```

### 4.2 Read/Write Structs with Timing (lines 99-126)

```zig
pub const Read = struct {
    callback: *const fn (read: *Storage.Read) void,
    buffer: []u8,
    zone: vsr.Zone,
    offset: u64,
    ready_at: Instant,  // When I/O completes
    stack_trace: StackTrace,
```

The `ready_at` field determines when the I/O operation completes, enabling latency simulation.

### 4.3 Memory Allocation (lines 198-253)

```zig
pub fn init(allocator: mem.Allocator, options: Storage.Options) !Storage {
    const sector_count = @divExact(options.size, constants.sector_size);
    const memory = try allocator.alignedAlloc(u8, constants.sector_size, options.size);
    var memory_written = try std.DynamicBitSetUnmanaged.initEmpty(allocator, sector_count);
    var faults = try std.DynamicBitSetUnmanaged.initEmpty(allocator, sector_count);
    // ...
}
```

### 4.4 Crash Simulation via reset() (lines 257-277)

```zig
pub fn reset(storage: *Storage) void {
    while (storage.writes.removeOrNull()) |write| {
        if (storage.prng.chance(storage.options.crash_fault_probability)) {
            // Corrupt random sector within in-flight write
            const sectors = SectorRange.from_zone(write.zone, write.offset, write.buffer.len);
            storage.fault_sector(write.zone, sectors.random(&storage.prng));
        }
    }
    while (storage.reads.removeOrNull()) |_| {}
    storage.next_tick_queue.reset();
}
```

On crash, pending writes may corrupt their target sectors—simulating torn writes.

### 4.5 Step Function - I/O Completion (lines 344-371)

```zig
pub fn step(storage: *Storage) bool {
    const read_ready_at_ns = if (storage.reads.peek()) |read| read.ready_at.ns else maxInt;
    const write_ready_at_ns = if (storage.writes.peek()) |write| write.ready_at.ns else maxInt;

    if (read_ready_at_ns <= storage.tick_instant().ns and
        read_ready_at_ns <= write_ready_at_ns) {
        const read = storage.reads.remove();
        storage.read_sectors_finish(read);
    } else if (write_ready_at_ns <= ...) {
        const write = storage.writes.remove();
        storage.write_sectors_finish(write);
    }
    // ...
}
```

I/O operations complete based on priority queue ordering by `ready_at` timestamp.

## 5. Fault Injection Mechanisms

### 5.1 Read Faults (lines 456-486)

```zig
fn read_sectors_finish(storage: *Storage, read: *Storage.Read) void {
    // Copy pristine data
    stdx.copy_disjoint(.exact, u8, read.buffer,
        storage.memory[offset_in_storage..][0..read.buffer.len]);

    // Probabilistically create new faults
    if (storage.prng.chance(storage.options.read_fault_probability)) {
        if (storage.pick_faulty_sector(read.zone, read.offset, read.buffer.len)) |sector| {
            storage.fault_sector(read.zone, sector);
        }
    }

    // Apply existing faults to returned data
    while (sectors.next()) |sector| {
        if (sector_corrupt) {
            // Deterministic bit-flip using pristine bytes as seed
            const corrupt_seed: u64 = @bitCast(sector_bytes[0..@sizeOf(u64)].*);
            var corrupt_prng = stdx.PRNG.from_seed(corrupt_seed);
            const corrupt_byte = corrupt_prng.index(sector_bytes);
            sector_bytes[corrupt_byte] ^= corrupt_prng.bit(u8);
        }
        if (sector_uninitialized) {
            storage.prng.fill(sector_bytes);  // Random data for unwritten sectors
        }
    }
}
```

Key design decisions:
- **Pristine memory preserved**: Faults only affect returned data
- **Deterministic corruption**: Same seed produces same corruption (read-retries don't help)
- **Localized bit-flip**: Single byte corrupted, not entire sector

### 5.2 Write Faults (lines 646-650)

```zig
if (storage.prng.chance(storage.options.write_fault_probability)) {
    if (storage.pick_faulty_sector(write.zone, write.offset, write.buffer.len)) |sector| {
        storage.fault_sector(write.zone, sector);
    }
}
```

Write faults:
1. Clear existing faults on successful write (line 642: `storage.faults.unset(sector)`)
2. Probabilistically re-fault after write completes

### 5.3 Write Misdirection (lines 603-638)

Write misdirection simulates disk firmware bugs where data lands at the wrong location.

```zig
const misdirect = storage.overlays.available() >= 2 and
    storage.pick_faulty_sector(write.zone, write.offset, write.buffer.len) != null and
    storage.prng.chance(storage.options.write_misdirect_probability);

if (misdirect_offset) |mistaken_offset| {
    // Create two overlays:
    // 1. Intended target: overlaid with OLD data
    // 2. Mistaken target: overlaid with NEW (write) data
    overlay_mistaken.* = .{ .zone = write.zone, .offset = mistaken_offset, .size = ... };
    overlay_intended.* = .{ .zone = write.zone, .offset = write.offset, .size = ... };

    stdx.copy_disjoint(.inexact, u8, overlay_mistaken_buffer, write.buffer);
    stdx.copy_disjoint(.inexact, u8, overlay_intended_buffer, target_intended_buffer);
}
```

Overlay system (lines 156-184):
- Maximum 2 overlays (one misdirect fault at a time)
- Misdirects are always **within the same zone**
- Entire write is misdirected (not partial)
- Pristine memory is updated; overlays track the "wrong" view

### 5.4 Crash Faults (lines 264-269)

```zig
if (storage.prng.chance(storage.options.crash_fault_probability)) {
    const sectors = SectorRange.from_zone(write.zone, write.offset, write.buffer.len);
    storage.fault_sector(write.zone, sectors.random(&storage.prng));
}
```

Simulates torn writes during crash—random sector within pending write gets corrupted.

## 6. ClusterFaultAtlas - Multi-Replica Coordination

The `ClusterFaultAtlas` (lines 1006-1216) coordinates faults across replicas to ensure cluster recovery is always possible.

### 6.1 Purpose

```
┌──────────────────────────────────────────────────────────────┐
│  Replica 0   │  Replica 1   │  Replica 2   │  Recovery?     │
├──────────────┼──────────────┼──────────────┼────────────────┤
│  Block A: OK │  Block A: OK │  Block A: X  │  YES (2/3)     │
│  Block B: X  │  Block B: OK │  Block B: OK │  YES (2/3)     │
│  Block C: X  │  Block C: X  │  Block C: OK │  YES (1/3)     │
│  Block D: X  │  Block D: X  │  Block D: X  │  NO! ❌        │
└──────────────────────────────────────────────────────────────┘
```

ClusterFaultAtlas prevents the last case by ensuring at least one replica has valid data.

### 6.2 Fault Distribution (line 1071)

```zig
const quorums = vsr.quorums(replica_count);
const faults_max = quorums.replication - 1;  // At most f faults where 2f+1 = replica_count
```

For each chunk (WAL sector, grid block, etc.), at most `faults_max` replicas can have faults.

### 6.3 Zone-Specific Rules

| Zone | Fault Rules |
|------|-------------|
| **superblock** | Never faulted by atlas (line 1125) - uses hash-chaining instead |
| **wal_headers** | Distributed faults via `faulty_wal_header_sectors` |
| **wal_prepares** | Coupled with wal_headers (same sectors) |
| **client_replies** | Distributed faults via `faulty_client_reply_slots` |
| **grid** | Disabled when `replica_count ≤ 2` (line 24) |

### 6.4 Initialization (lines 1077-1101)

```zig
for (0..chunks[0].bit_length) |chunk| {
    var replicas: ReplicaSet = .{};
    while (replicas.count() < faults_max) {
        const replica_index = prng.int_inclusive(u8, replica_count - 1);
        if (chunks[replica_index].count() + 1 < chunks[replica_index].capacity()) {
            chunks[replica_index].set(chunk);
            replicas.set(replica_index);
        }
    }
}
```

For each chunk, randomly select up to `faults_max` replicas to mark as faulty.

## 7. Latency Simulation

### 7.1 Exponential Distribution (line 684)

```zig
fn latency(storage: *Storage, min: Duration, mean: Duration) Duration {
    return .{ .ns = @max(min.ns, fuzz.random_int_exponential(&storage.prng, u64, mean.ns)) };
}
```

### 7.2 Configuration (lines 663-685)

```zig
fn read_latency(storage: *Storage) Duration {
    return storage.latency(
        storage.options.read_latency_min,
        storage.options.read_latency_mean,
    );
}
```

### 7.3 Priority Queue Ordering (lines 190-191)

```zig
reads: std.PriorityQueue(*Storage.Read, void, Storage.Read.less_than),
writes: std.PriorityQueue(*Storage.Write, void, Storage.Write.less_than),
```

Operations complete in order of their `ready_at` timestamp.

## 8. Storage Checker

The `StorageChecker` (`storage_checker.zig`) validates storage determinism across replicas.

### 8.1 Verification Points

| Event | What's Checked |
|-------|----------------|
| **Compaction bar** | Grid blocks checksum (lines 121-160) |
| **Checkpoint** | Superblock, client_replies, grid (lines 162-182) |
| **Sync complete** | Superblock, grid (lines 184-202) |

### 8.2 Cross-Replica Validation (lines 245-269)

```zig
if (checker.checkpoints.getPtr(op_checkpoint)) |checkpoint_expect| {
    for (std.enums.values(CheckpointArea)) |area| {
        const checksum_actual = checkpoint_actual.get(area) orelse continue;
        if (checkpoint_expect.fetchPut(area, checksum_actual)) |checksum_expect| {
            if (checksum_expect != checksum_actual) {
                return error.StorageMismatch;
            }
        }
    }
} else {
    // First replica to reach checkpoint becomes reference
    try checker.checkpoints.putNoClobber(op_checkpoint, checkpoint_actual);
}
```

### 8.3 Grid Checksum Calculation (lines 382-457)

```zig
fn checksum_grid(checker: *StorageChecker, ...) u128 {
    var stream = vsr.ChecksumStream.init();

    while (blocks_acquired.next()) |block_address_index| {
        const block_address: u64 = block_address_index + 1;
        if (free_set.is_released(block_address)) continue;  // Skip released blocks

        const block = superblock.storage.grid_block(block_address).?;
        stream.add(block[0..block_header.size]);
        stream.add(std.mem.asBytes(&block_address));  // Guard against identical blocks
    }
    return stream.checksum();
}
```

## 9. Fuzz Testing

The `storage_fuzz.zig` harness tests the LSE recovery mechanism.

### 9.1 Fault Clustering (lines 31-51)

```zig
const failed_sector_cluster_count = prng.range_inclusive(usize, 1, 10);
const failed_sector_cluster_minimum_length = prng.range_inclusive(usize, 1, 3);
const failed_sector_cluster_maximum_length = ... + prng.range_inclusive(usize, 1, 3);

for (0..failed_sector_cluster_count) |_| {
    fault_map.setRangeValue(.{ .start = start, .end = end }, true);
}
```

Simulates realistic fault patterns with spatial locality.

### 9.2 Triple-Check Verification (lines 168-178)

```zig
try std.testing.expectEqualSlices(u8,
    storage_data_written[start..end],
    storage_data_stored[start..end],
);
try std.testing.expectEqualSlices(u8,
    storage_data_stored[start..end],
    storage_data_read[start..end],
);
```

Verifies: `written == stored == read`

## 10. Configuration Options

| Option | Purpose | Default |
|--------|---------|---------|
| `read_latency_min` | Minimum read latency | 0ns |
| `read_latency_mean` | Mean read latency (exponential) | 0ns |
| `write_latency_min` | Minimum write latency | 0ns |
| `write_latency_mean` | Mean write latency (exponential) | 0ns |
| `read_fault_probability` | Probability of read corruption | 0% |
| `write_fault_probability` | Probability of write corruption | 0% |
| `write_misdirect_probability` | Probability of misdirected write | 0% |
| `crash_fault_probability` | Probability of crash corrupting pending write | 0% |

## 11. Code Mapping for Moonpool

| TigerBeetle Component | Purpose | Moonpool Equivalent |
|-----------------------|---------|---------------------|
| `StorageType(IO)` | Storage interface | `trait Storage<IO>` |
| `Storage.Read/Write` | I/O request structs | `ReadRequest`/`WriteRequest` |
| `testing/storage.Storage` | Simulated storage | `SimulatedStorage` |
| `Options` | Fault configuration | `StorageFaultConfig` |
| `ClusterFaultAtlas` | Multi-replica faults | `ClusterFaultMap` |
| `StorageChecker` | Invariant validation | `StorageInvariant` |
| `memory` + `faults` bitset | Clean data + fault tracking | Separate pristine storage + fault bitmap |
| `overlays` | Misdirection simulation | `MisdirectOverlay` |

## 12. Key Design Patterns

### 12.1 Pristine Memory Separation

TigerBeetle keeps `memory` pristine and tracks faults separately:
- Allows toggling faults without data loss
- Enables deterministic corruption (same seed = same corruption)
- Simplifies misdirection tracking via overlays

### 12.2 Quorum-Aware Fault Distribution

`ClusterFaultAtlas` ensures:
- At most `replication_quorum - 1` replicas fault any given chunk
- At least one replica always has valid data
- Single-replica clusters only get crash faults (no read/write faults)

### 12.3 Priority Queue for I/O Ordering

Both reads and writes use priority queues ordered by `ready_at`:
- Natural ordering by completion time
- Enables realistic interleaving
- Supports configurable latency distributions

### 12.4 Zone-Based Fault Tolerance

Different storage zones have different fault tolerance requirements:
- **Superblock**: Multiple copies with hash-chaining
- **WAL**: Redundant headers + prepares, repairable from peers
- **Grid**: Block-level redundancy across replicas
