# FoundationDB File Simulation

This document analyzes how FoundationDB simulates file I/O for deterministic testing, including power failure simulation, chaos fault injection, and disk timing.

## Reference Files

- `docs/references/foundationdb/IAsyncFile.h` - Base file interface
- `docs/references/foundationdb/AsyncFileChaos.h` - Chaos injection (bit flips, delays)
- `docs/references/foundationdb/AsyncFileNonDurable.actor.h` - Power failure simulation (excerpt)
- `docs/references/foundationdb/SimulatorMachineInfo.h` - Machine state including open files
- `docs/references/foundationdb/ChaosMetrics.h` - Disk failure injection types
- `docs/references/foundationdb/sim2-file.actor.cpp` - SimpleFile and Sim2FileSystem (excerpt)

## Architecture Overview

### Decorator Pattern Stack

FDB uses a decorator pattern to layer file simulation behaviors. When a file is opened in simulation, it goes through this stack (bottom to top):

```
┌─────────────────────────────────────┐
│   AsyncFileEncrypted (optional)     │  ← Encryption layer
├─────────────────────────────────────┤
│   AsyncFileChaos                    │  ← Bit flips, disk delays
├─────────────────────────────────────┤
│   AsyncFileDetachable               │  ← Shutdown/kill handling
├─────────────────────────────────────┤
│   AsyncFileNonDurable               │  ← Power failure simulation
├─────────────────────────────────────┤
│   AsyncFileWriteChecker (optional)  │  ← Checksum verification
├─────────────────────────────────────┤
│   SimpleFile                        │  ← Disk timing simulation
├─────────────────────────────────────┤
│   Actual File System                │  ← Real disk I/O
└─────────────────────────────────────┘
```

This stack is constructed in `Sim2FileSystem::open()` (see `sim2-file.actor.cpp:3087-3142`).

## Component Details

### 1. IAsyncFile Interface

**File:** `IAsyncFile.h`

The base contract for all async file operations:

```cpp
class IAsyncFile {
public:
    // Core operations
    virtual Future<int> read(void* data, int length, int64_t offset) = 0;
    virtual Future<Void> write(void const* data, int length, int64_t offset) = 0;
    virtual Future<Void> truncate(int64_t size) = 0;
    virtual Future<Void> sync() = 0;
    virtual Future<int64_t> size() const = 0;
    virtual std::string getFilename() const = 0;

    // Open flags (key ones for simulation)
    enum {
        OPEN_ATOMIC_WRITE_AND_CREATE = 0x80000,  // Temp file, renamed on sync
        OPEN_NO_AIO = 0x200000,                  // Don't use kernel AIO
        OPEN_UNCACHED = 0x20000,                 // Bypass cache (triggers sim stack)
    };
};
```

**Key insight:** `OPEN_UNCACHED` triggers the full simulation stack. Without it, files use `AsyncFileCached` instead.

### 2. SimpleFile - Disk Timing Simulation

**File:** `sim2-file.actor.cpp:659-1001`

SimpleFile wraps actual file I/O with realistic disk timing delays.

#### Disk Parameters

```cpp
struct DiskParameters {
    double nextOperation;   // When the next operation can start
    int64_t iops;          // Default: 25,000 IOPS
    int64_t bandwidth;     // Default: 150 MB/s (150,000,000 bytes/s)
};
```

#### Delay Calculation

```cpp
Future<Void> waitUntilDiskReady(Reference<DiskParameters> diskParameters, int64_t size, bool sync) {
    // Failed disk never completes
    if (g_simulator->getCurrentProcess()->failedDisk) {
        return Never();
    }

    // Delay = (1/iops) + (size/bandwidth)
    diskParameters->nextOperation += (1.0 / diskParameters->iops) + (size / diskParameters->bandwidth);

    // Add random latency
    double randomLatency;
    if (sync) {
        // Sync has higher latency: 5-15ms normally, up to 1s with BUGGIFY
        randomLatency = .005 + deterministicRandom()->random01() * (BUGGIFY ? 1.0 : .010);
    } else {
        randomLatency = 10 * deterministicRandom()->random01() / diskParameters->iops;
    }

    return delayUntil(diskParameters->nextOperation + randomLatency);
}
```

**Example timing for 4KB write:**
- IOPS component: 1/25000 = 0.00004s = 40μs
- Bandwidth component: 4096/150000000 = 0.000027s = 27μs
- Total base delay: ~67μs + random latency

#### Atomic Write and Create

Files opened with `OPEN_ATOMIC_WRITE_AND_CREATE`:
1. Write to `filename.part` temporary file
2. On first `sync()`, atomically rename to actual filename
3. Simulates safe file creation patterns

### 3. AsyncFileNonDurable - Power Failure Simulation

**File:** `AsyncFileNonDurable.actor.h`

This is the core of FDB's crash simulation. It simulates what happens when power fails during writes.

#### KillMode - Corruption Levels

```cpp
enum KillMode {
    NO_CORRUPTION = 0,    // All writes succeed (used during durable sync)
    DROP_ONLY = 1,        // Writes can be dropped entirely
    FULL_CORRUPTION = 2   // Writes can be dropped OR corrupted
};
```

Each file randomly chooses `killMode = randomInt(1, 3)` at open time, determining its worst-case corruption behavior.

#### Write Corruption Mechanics

When a file is "killed" (simulated power failure), writes are processed at two granularities:
- **Page level (4KB):** Determines if entire page is affected
- **Sector level (512B):** Determines corruption within affected pages

For each sector during a non-durable write:

| killMode | Probability | Result |
|----------|------------|--------|
| NO_CORRUPTION | 100% | Write correctly |
| DROP_ONLY | 50% | Drop the write |
| FULL_CORRUPTION | 25% | Write correctly |
| FULL_CORRUPTION | 50% (of remaining 75%) | Corrupt the write |
| FULL_CORRUPTION | 50% (of remaining 75%) | Drop the write |

#### Corruption Types

When a write is corrupted (from `AsyncFileNonDurable.actor.h:456-508`):

```cpp
// The incorrect part can be:
// - rightmost bytes (side = 0)
// - leftmost bytes (side = 1)
// - entire sector (side = 2)
int side = deterministicRandom()->randomInt(0, 3);

// 50% chance of garbage (100% if entire sector is bad)
bool garbage = side == 2 || deterministicRandom()->random01() < 0.5;

// If garbage, fill with random bytes
for (int i = 0; i < badEnd - badStart; i += sizeof(uint32_t)) {
    uint32_t val = deterministicRandom()->randomUInt32();
    memcpy(&badData[i], &val, ...);
}
```

#### Kill Durability

When `kill()` is called (see `AsyncFileNonDurable.actor.h:605-677`):

```cpp
// 10% chance that writes become durable even during kill
bool writeDurable = durable || deterministicRandom()->random01() < 0.1;

// If previously synced and writes happened to be durable, 50% chance to actually sync
if (self->hasBeenSynced && writeDurable && deterministicRandom()->random01() < 0.5) {
    wait(self->file->sync());
}
```

This models real-world scenarios where some writes may persist even during power failure.

### 4. AsyncFileChaos - Chaos Fault Injection

**File:** `AsyncFileChaos.h`

Injects runtime chaos events for testing resilience.

#### File Filtering

Chaos is **only enabled** for storage files:
```cpp
enabled = file->getFilename().find("storage-") != std::string::npos &&
          file->getFilename().find("sqlite-wal") == std::string::npos;
```

This protects critical metadata files from corruption while testing data file handling.

#### Bit Flips

Random bit corruption during writes:

```cpp
auto bitFlipPercentage = BitFlipper::flipper()->getBitFlipPercentage();
if (deterministicRandom()->random01() < bitFlipPercentage / 100) {
    // Copy data and flip a random bit
    pdata = (char*)arena.allocate4kAlignedBuffer(length);
    memcpy(pdata, data, length);

    auto corruptedPos = deterministicRandom()->randomInt(0, length);
    pdata[corruptedPos] ^= (1 << deterministicRandom()->randomInt(0, 8));

    // Track which 4KB block was corrupted
    corruptedBlock = (offset + corruptedPos) / 4096;
    g_simulator->corruptedBlocks.emplace(file->getFilename(), corruptedBlock);
}
```

The `corruptedBlocks` set tracks which blocks have been corrupted, allowing tests to verify that the system detects or handles corruption.

#### Disk Delays

```cpp
double getDelay() const {
    if (!enabled) return 0.0;

    auto res = g_network->global(INetwork::enDiskFailureInjector);
    if (res) {
        DiskFailureInjector* delayInjector = static_cast<DiskFailureInjector*>(res);
        return delayInjector->getDiskDelay();
    }
    return 0.0;
}
```

Delays are applied before each I/O operation when `DiskFailureInjector` is active.

### 5. AsyncFileWriteChecker - Checksum Verification

**File:** `AsyncFileWriteChecker.actor.h` (in FDB source, not included in excerpts)

Optional layer that:
1. Computes CRC32 checksum on writes
2. Stores checksums in an LRU cache
3. Verifies checksums on reads
4. Detects silent data corruption

Enabled via `FLOW_KNOBS->PAGE_WRITE_CHECKSUM_HISTORY > 0`.

### 6. AsyncFileDetachable - Shutdown Handling

Wraps files to handle clean shutdown vs kill scenarios:
- On shutdown: Detaches from underlying file
- Subsequent operations throw `io_error` as injected fault

### 7. DiskFailureInjector - Configurable Chaos

**File:** `ChaosMetrics.h`

Provides two types of disk failures:

```cpp
struct DiskFailureInjector {
    // Stalls: Disk completely stops for duration
    double stallInterval;   // How often to stall (0 = once)
    double stallDuration;   // How long each stall lasts

    // Slowdown: Random delays on each operation
    double throttlePeriod;  // Duration of slowdown period

    double getDiskDelay() const;  // Returns delay for next operation
};
```

## Machine State

**File:** `SimulatorMachineInfo.h`

Each simulated machine tracks:

```cpp
struct MachineInfo {
    // Open files on this machine (shared across processes)
    std::map<std::string, UnsafeWeakFutureReference<IAsyncFile>> openFiles;

    // Files being deleted or closed
    std::set<std::string> deletingOrClosingFiles;
    std::set<std::string> closingFiles;
};
```

This allows simulation of:
- Multiple processes accessing same file
- File handle cleanup during machine reboot
- Proper file deletion semantics

## Configuration Knobs

Key simulation knobs (from `flow/Knobs.cpp`):

| Knob | Default | Description |
|------|---------|-------------|
| `SIM_DISK_IOPS` | 25000 | Simulated disk IOPS |
| `SIM_DISK_BANDWIDTH` | 150000000 | Simulated bandwidth (bytes/s) |
| `MIN_OPEN_TIME` | 0.0002 | Minimum file open delay |
| `MAX_OPEN_TIME` | 0.001 | Maximum file open delay |
| `NON_DURABLE_MAX_WRITE_DELAY` | 0.2 | Max write delay before sync |
| `PAGE_WRITE_CHECKSUM_HISTORY` | 0 | Enable checksum verification |
| `ENABLE_CHAOS_FEATURES` | false | Enable bit flips/delays |

## Code Mapping for Moonpool

| FDB Component | Purpose | Moonpool Equivalent |
|---------------|---------|---------------------|
| `IAsyncFile` | File interface | `trait AsyncFile` |
| `SimpleFile` | Disk timing | `SimulatedFile` with `DiskParameters` |
| `AsyncFileNonDurable` | Power failure | `NonDurableFile` with `KillMode` |
| `AsyncFileChaos` | Bit flips/delays | `ChaosFile` wrapper |
| `DiskParameters` | IOPS/bandwidth | `DiskConfig` struct |
| `waitUntilDiskReady` | Timing delay | `disk_delay()` async function |
| `MachineInfo::openFiles` | File tracking | Per-machine file registry |
| `corruptedBlocks` | Corruption tracking | `HashSet<(String, u64)>` in SimWorld |

## Implementation Considerations

### 1. Decorator vs Trait Objects

FDB uses C++ virtual dispatch through `Reference<IAsyncFile>`. In Rust, consider:
- Trait objects (`Box<dyn AsyncFile>`) for runtime polymorphism
- Generics with trait bounds for static dispatch (better performance)
- Enum-based dispatch for fixed set of wrappers

### 2. Deterministic Random

All chaos injection uses `deterministicRandom()` which is seeded by the simulation. Ensure Moonpool's random provider is used consistently.

### 3. Kill Semantics

The `kill()` operation is tricky:
- Must interrupt in-flight writes
- Some writes may partially complete
- Corruption happens at sector (512B) granularity
- 10% chance writes are actually durable

### 4. File Caching

FDB caches open files per machine (`MachineInfo::openFiles`). This is important for:
- Atomic rename operations
- Ensuring processes share file state
- Tracking corruption across reopens

### 5. Chaos Filtering

Only enable chaos on data files, not metadata:
- Protects coordination/lock files
- Tests recovery with corrupted data
- Matches real-world failure patterns

## Testing Scenarios

This file simulation enables testing:

1. **Power failure during write** - Partial writes, sector corruption
2. **Disk full** - Via `failedDisk` flag
3. **Slow disk** - Via `DiskFailureInjector` throttling
4. **Bit rot** - Via `BitFlipper` random corruption
5. **Disk stalls** - Via `DiskFailureInjector` stalls
6. **Atomic file creation** - Via `.part` file pattern
7. **Concurrent access** - Via shared `MachineInfo::openFiles`
