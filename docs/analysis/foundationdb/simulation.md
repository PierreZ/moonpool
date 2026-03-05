# FoundationDB Deterministic Simulation

How FDB replaces real I/O with virtual time and memory-based connections/files for deterministic testing.

## Overview

```
Production Runtime                  Simulation Runtime
┌──────────────┐                    ┌──────────────┐
│   Net2       │  ──replaced by──>  │   Sim2       │
│  (real TCP,  │                    │  (virtual    │
│   real time, │                    │   time,      │
│   real AIO)  │                    │   in-memory  │
└──────────────┘                    │   conns)     │
                                    └──────────────┘

Real File Stack                     Simulated File Stack
┌──────────────┐                    ┌──────────────────────────┐
│   AsyncFileKAIO / POSIX │         │ AsyncFileEncrypted (opt) │
└──────────────┘                    │ AsyncFileChaos           │
                                    │ AsyncFileDetachable      │
                                    │ AsyncFileNonDurable      │
                                    │ AsyncFileWriteChecker    │
                                    │ SimpleFile (disk timing) │
                                    │ Actual File System       │
                                    └──────────────────────────┘
```

Both `Net2` and `Sim2` implement the `INetwork` interface. Application code is written against `INetwork` and never knows which runtime it is using. The file stack is assembled at open time by `Sim2FileSystem::open()`.

**Source files:**
- `fdbrpc/sim2.actor.cpp` -- Sim2 network, virtual time, Sim2Conn
- `fdbrpc/sim2-file.actor.cpp` -- SimpleFile, Sim2FileSystem
- `flow/include/flow/IAsyncFile.h` -- Base file interface
- `fdbrpc/AsyncFileNonDurable.actor.h` -- Power failure simulation
- `fdbrpc/AsyncFileChaos.h` -- Chaos injection

---

## Sim2: Network Simulation

### Virtual Time

Time is a `double` (seconds) that advances **discretely** via the event loop -- never by wall clock:

```cpp
class Sim2 : public ISimulator, public INetwork, public INetworkConnections {
    double currentTime;  // Virtual time

    double now() const override {
        return currentTime;  // NOT real time!
    }

    bool isSimulated() const override { return true; }
};
```

Time is frozen while a coroutine executes. It advances only when the scheduler pops the next event. Simulations typically achieve a **10:1 real-to-simulated time ratio** -- when no CPU work is pending, the simulator fast-forwards to the next scheduled event.

### Event Queue

The simulator uses a **min-priority queue** ordered by scheduled time:

```cpp
struct Task {
    double time;           // When to execute
    TaskPriority priority; // Secondary ordering
    Action action;         // The continuation
};

// Time advancement algorithm
while (hasMoreEvents()) {
    Task task = taskQueue.pop();      // Get minimum time task
    currentTime = task.time;           // ADVANCE TIME
    onProcess(task.process);           // Switch context
    task.action();                     // Execute
}
```

**Event ordering at the same timestamp:**

1. **Time** -- primary sort key
2. **TaskPriority** -- determines order within the same time
3. **Insertion order** (FIFO) -- tiebreaker for same priority

```cpp
enum class TaskPriority {
    Max,                    // Highest
    RunCycleFunction,
    FlushTrace,
    ReadSocket,
    WriteSocket,
    AcceptSocket,
    DefaultYield,
    DefaultEndpoint,
    DataDistribution,
    UpdateStorage,
    Zero,                   // Lowest
    Min
};
```

Time advancement is **blocked** when the ready queue has tasks at current time or a coroutine is executing.

### Sim2Conn

`Sim2Conn` simulates TCP connections using **in-memory byte queues**:

```cpp
class Sim2Conn : public IConnection, ReferenceCounted<Sim2Conn> {
    // Process context
    ISimulator::ProcessInfo* process;
    ISimulator::ProcessInfo* peerProcess;
    Reference<Sim2Conn> peerConn;        // The other endpoint

    // Byte tracking for data pipeline
    AsyncVar<int64_t> sentBytes;         // Bytes written by sender
    AsyncVar<int64_t> receivedBytes;     // Bytes available to read
    int64_t writtenBytes;                // Total written

    // Buffers
    std::queue<std::pair<int64_t, std::string>> unsent;
    std::string recvBuffer;

    // State
    bool closed;
    UID id;
    NetworkAddress peerAddress;

    // Async notification
    Future<Void> receiverActor;
};
```

Each connection is one half of a **paired** bidirectional link. The `peerConn` pointer cross-links the two endpoints. Data flow is driven by the receiver actor, not by direct buffer copying.

### Receiver Actor

The receiver actor is the core transfer mechanism. It runs on the **peer** process and simulates partial delivery plus latency:

```cpp
ACTOR static Future<Void> receiver(Sim2Conn* self) {
    loop {
        // Wait to run on peer process (context switch simulation)
        if (self->sentBytes.get() != self->receivedBytes.get())
            wait(g_simulator->onProcess(self->peerProcess));

        // Wait for bytes to be sent
        while (self->sentBytes.get() == self->receivedBytes.get())
            wait(self->sentBytes.onChange());

        ASSERT(g_simulator->getCurrentProcess() == self->peerProcess);

        // Randomly decide how many bytes to "receive" (partial delivery)
        state int64_t pos = deterministicRandom()->random01() < .5 ?
            deterministicRandom()->randomInt64(self->receivedBytes.get(),
                                               self->sentBytes.get()) :
            self->sentBytes.get();

        // Inject network latency
        wait(delay(deterministicRandom()->random01() * FLOW_KNOBS->MAX_CLOGGING_DELAY));

        // Update received bytes (triggers onReadable())
        self->receivedBytes.set(pos);
    }
}
```

The 50/50 split between partial and full delivery exercises TCP-like behavior where reads may return fewer bytes than were sent.

### Simulated Connection Establishment

```cpp
ACTOR static Future<Reference<IConnection>> connect(NetworkAddress toAddr) {
    ISimulator::ProcessInfo* peerProcess = g_simulator->getProcessByAddress(toAddr);

    if (!peerProcess || peerProcess->failed) {
        wait(delay(FLOW_KNOBS->CONNECTION_FAILURE_DELAY));
        throw connection_failed();
    }

    // Create paired connections (bidirectional)
    state Reference<Sim2Conn> self(new Sim2Conn());
    state Reference<Sim2Conn> peer(new Sim2Conn());

    // Cross-link
    self->peerProcess = peerProcess;
    self->peerConn = peer;
    peer->peerProcess = g_simulator->getCurrentProcess();
    peer->peerConn = self;

    // Set random latency for this connection pair
    auto latency = g_clogging.setPairLatencyIfNotSet(
        peerProcess->address.ip, process->address.ip,
        FLOW_KNOBS->MAX_CLOGGING_LATENCY * deterministicRandom()->random01());

    // Simulate connection delay
    wait(delay(deterministicRandom()->random01() * FLOW_KNOBS->SIM_CONNECT_DELAY));

    // Register with listener (simulates accept())
    g_simulator->connections[toAddr].send({g_network->getLocalAddress(), peer});

    // Start receiver actors for both directions
    self->receiverActor = receiver(self.getPtr());
    peer->receiverActor = receiver(peer.getPtr());

    return self;
}
```

If the target process does not exist or has failed, the connection attempt waits a delay then throws -- simulating a TCP timeout. Pair-level latency is assigned once and reused for the connection lifetime.

### Connection Close Behavior

```cpp
void close() override {
    if (!closed) {
        closed = true;
        receiverActor.cancel();          // Stop receiver

        if (peerConn) {
            peerConn->peerClosed = true;
            peerConn->closedPromise.sendError(connection_failed());
        }

        unsent = {};                      // Discard unsent data
        recvBuffer.clear();               // Clear receive buffer
    }
}
```

**In-flight data handling:**

| Category | Outcome |
|----------|---------|
| Unsent data | Discarded from sender's buffer |
| In-transit data | Lost (simulating real network) |
| Received but unread | Remains briefly, then `connection_failed` raised |
| Outstanding futures | Cancelled with `connection_failed` |

### Latency Knobs

```cpp
// From Knobs.cpp
init(MIN_NETWORK_LATENCY,           100e-6);   // 100 microseconds
init(FAST_NETWORK_LATENCY,          800e-6);   // 800 microseconds
init(SLOW_NETWORK_LATENCY,          100e-3);   // 100 milliseconds
init(MAX_CLOGGING_LATENCY,          0);        // Buggified: up to 0.1s
init(MAX_BUGGIFIED_DELAY,           0);        // Buggified: up to 0.2s
```

`MAX_CLOGGING_LATENCY` and `MAX_BUGGIFIED_DELAY` default to zero but are set to non-zero values when BUGGIFY is enabled, introducing realistic jitter.

---

## Network Topology and Partitions

### Cluster Hierarchy

```
Cluster
+-- Datacenter 1
|   +-- Machine 1
|   |   +-- Process 1 (StorageServer)
|   |   +-- Process 2 (TLog)
|   +-- Machine 2
|       +-- ...
+-- Datacenter 2
|   +-- ...
+-- Satellites
```

### ProcessInfo

```cpp
struct ProcessInfo {
    NetworkAddress address;
    LocalityData locality;       // Zone, DC, machine
    ProcessClass processClass;   // Role
    Reference<IListener> listener;

    double diskReliability;
    double memoryLimit;
    bool failed;
};
```

Each process has a unique `NetworkAddress` and locality metadata. The `failed` flag causes connection attempts to that process to fail after a delay.

### Partition Types

| Type | Description |
|------|-------------|
| Machine-level | Single machine isolated from the cluster |
| Rack-level | Entire rack partitioned |
| Datacenter-level | Full DC partitioned |
| Asymmetric | A can reach B, but B cannot reach A |

```cpp
void clogTlog(double seconds) {
    for (const auto& process : g_simulator->getAllProcesses()) {
        // g_simulator methods:
        // - clogInterface() -- clog specific interface
        // - clogPair() -- clog connection between addresses
    }
}
```

---

## File Simulation

### IAsyncFile Interface

**Source:** `flow/include/flow/IAsyncFile.h`

```cpp
class IAsyncFile {
public:
    virtual Future<int> read(void* data, int length, int64_t offset) = 0;
    virtual Future<Void> write(void const* data, int length, int64_t offset) = 0;
    virtual Future<Void> truncate(int64_t size) = 0;
    virtual Future<Void> sync() = 0;
    virtual Future<int64_t> size() const = 0;
    virtual std::string getFilename() const = 0;

    enum {
        OPEN_ATOMIC_WRITE_AND_CREATE = 0x80000,  // Temp file, renamed on sync
        OPEN_NO_AIO = 0x200000,                  // Don't use kernel AIO
        OPEN_UNCACHED = 0x20000,                 // Bypass cache (triggers sim stack)
    };
};
```

The `OPEN_UNCACHED` flag triggers the full simulation decorator stack. Without it, files use `AsyncFileCached` instead.

### Decorator Pattern Stack

Constructed in `Sim2FileSystem::open()` (`sim2-file.actor.cpp:3087-3142`):

```
+-------------------------------------+
|   AsyncFileEncrypted (optional)     |  <-- Encryption layer
+-------------------------------------+
|   AsyncFileChaos                    |  <-- Bit flips, disk delays
+-------------------------------------+
|   AsyncFileDetachable               |  <-- Shutdown/kill handling
+-------------------------------------+
|   AsyncFileNonDurable               |  <-- Power failure simulation
+-------------------------------------+
|   AsyncFileWriteChecker (optional)  |  <-- Checksum verification
+-------------------------------------+
|   SimpleFile                        |  <-- Disk timing simulation
+-------------------------------------+
|   Actual File System                |  <-- Real disk I/O
+-------------------------------------+
```

Each layer wraps the one below, adding a specific fault dimension.

### SimpleFile -- Disk Timing

**Source:** `sim2-file.actor.cpp:659-1001`

```cpp
struct DiskParameters {
    double nextOperation;   // When the next operation can start
    int64_t iops;          // Default: 25,000 IOPS
    int64_t bandwidth;     // Default: 150 MB/s (150,000,000 bytes/s)
};
```

Delay calculation models both IOPS and bandwidth costs:

```cpp
Future<Void> waitUntilDiskReady(Reference<DiskParameters> diskParameters,
                                 int64_t size, bool sync) {
    if (g_simulator->getCurrentProcess()->failedDisk) {
        return Never();  // Failed disk never completes
    }

    // Delay = (1/iops) + (size/bandwidth)
    diskParameters->nextOperation +=
        (1.0 / diskParameters->iops) + (size / diskParameters->bandwidth);

    double randomLatency;
    if (sync) {
        // Sync: 5-15ms normally, up to 1s with BUGGIFY
        randomLatency = .005 + deterministicRandom()->random01() * (BUGGIFY ? 1.0 : .010);
    } else {
        randomLatency = 10 * deterministicRandom()->random01() / diskParameters->iops;
    }

    return delayUntil(diskParameters->nextOperation + randomLatency);
}
```

**Example: 4KB write** -- IOPS component: 1/25000 = 40us, bandwidth component: 4096/150000000 = 27us, total base delay ~67us plus random jitter.

Files opened with `OPEN_ATOMIC_WRITE_AND_CREATE` write to `filename.part`, then atomically rename on first `sync()`.

### AsyncFileNonDurable -- Power Failure

**Source:** `fdbrpc/AsyncFileNonDurable.actor.h`

Each file randomly chooses a `killMode` at open time:

```cpp
enum KillMode {
    NO_CORRUPTION = 0,    // All writes succeed
    DROP_ONLY = 1,        // Writes can be dropped entirely
    FULL_CORRUPTION = 2   // Writes can be dropped OR corrupted
};
```

When a file is "killed" (simulated power failure), writes are processed at **sector granularity (512 bytes)**:

| killMode | Probability | Result |
|----------|------------|--------|
| NO_CORRUPTION | 100% | Write correctly |
| DROP_ONLY | 50% | Drop the write |
| FULL_CORRUPTION | 25% | Write correctly |
| FULL_CORRUPTION | ~37.5% | Corrupt the write |
| FULL_CORRUPTION | ~37.5% | Drop the write |

**Corruption types** -- for each corrupted sector:

```cpp
// Which part of the sector is bad:
// side=0: rightmost bytes, side=1: leftmost bytes, side=2: entire sector
int side = deterministicRandom()->randomInt(0, 3);

// 50% chance of garbage (100% if entire sector)
bool garbage = side == 2 || deterministicRandom()->random01() < 0.5;

// Fill bad region with random bytes
for (int i = 0; i < badEnd - badStart; i += sizeof(uint32_t)) {
    uint32_t val = deterministicRandom()->randomUInt32();
    memcpy(&badData[i], &val, ...);
}
```

**Kill durability** -- even during a kill, some writes may persist:

```cpp
// 10% chance writes become durable even during kill
bool writeDurable = durable || deterministicRandom()->random01() < 0.1;

// If previously synced and durable, 50% chance to actually sync
if (self->hasBeenSynced && writeDurable && deterministicRandom()->random01() < 0.5) {
    wait(self->file->sync());
}
```

This models real hardware where disk write caches may flush during power loss.

### AsyncFileChaos -- Chaos Injection

**Source:** `fdbrpc/AsyncFileChaos.h`

Only enabled for storage data files (not WAL or metadata):

```cpp
enabled = file->getFilename().find("storage-") != std::string::npos &&
          file->getFilename().find("sqlite-wal") == std::string::npos;
```

**Bit flips** -- random single-bit corruption during writes:

```cpp
auto bitFlipPercentage = BitFlipper::flipper()->getBitFlipPercentage();
if (deterministicRandom()->random01() < bitFlipPercentage / 100) {
    pdata = (char*)arena.allocate4kAlignedBuffer(length);
    memcpy(pdata, data, length);

    auto corruptedPos = deterministicRandom()->randomInt(0, length);
    pdata[corruptedPos] ^= (1 << deterministicRandom()->randomInt(0, 8));

    // Track corruption for test verification
    corruptedBlock = (offset + corruptedPos) / 4096;
    g_simulator->corruptedBlocks.emplace(file->getFilename(), corruptedBlock);
}
```

**Disk delays** are injected via `DiskFailureInjector` when active, adding delay before each I/O operation.

### AsyncFileWriteChecker -- Checksum Verification

Optional layer (enabled when `PAGE_WRITE_CHECKSUM_HISTORY > 0`) that computes CRC32 on writes, stores checksums in an LRU cache, and verifies on reads. Detects silent data corruption.

### AsyncFileDetachable -- Shutdown Handling

Wraps files for clean kill handling. On shutdown, detaches from the underlying file so subsequent operations throw `io_error`.

### DiskFailureInjector -- Configurable Stalls/Slowdowns

**Source:** `fdbrpc/ChaosMetrics.h`

```cpp
struct DiskFailureInjector {
    double stallInterval;   // How often to stall (0 = once)
    double stallDuration;   // How long each stall lasts
    double throttlePeriod;  // Duration of slowdown period

    double getDiskDelay() const;  // Returns delay for next operation
};
```

Stalls simulate complete disk hangs; throttling simulates degraded disk performance.

---

## Machine State

**Source:** `fdbrpc/SimulatorMachineInfo.h`

```cpp
struct MachineInfo {
    std::map<std::string, UnsafeWeakFutureReference<IAsyncFile>> openFiles;
    std::set<std::string> deletingOrClosingFiles;
    std::set<std::string> closingFiles;
};
```

Open files are tracked per machine (shared across processes on the same machine). This enables:

- Multiple processes accessing the same file
- File handle cleanup during machine reboot
- Proper file deletion semantics (delete-while-open)
- Corruption tracking across file reopens

---

## Configuration Knobs

| Knob | Default | Description |
|------|---------|-------------|
| `SIM_DISK_IOPS` | 25000 | Simulated disk IOPS |
| `SIM_DISK_BANDWIDTH` | 150000000 | Simulated bandwidth (bytes/s) |
| `MIN_OPEN_TIME` | 0.0002 | Minimum file open delay (s) |
| `MAX_OPEN_TIME` | 0.001 | Maximum file open delay (s) |
| `NON_DURABLE_MAX_WRITE_DELAY` | 0.2 | Max write delay before sync (s) |
| `PAGE_WRITE_CHECKSUM_HISTORY` | 0 | Checksum LRU size (0 = disabled) |
| `ENABLE_CHAOS_FEATURES` | false | Enable bit flips/delays |
| `MIN_NETWORK_LATENCY` | 100e-6 | 100 microseconds |
| `FAST_NETWORK_LATENCY` | 800e-6 | 800 microseconds |
| `SLOW_NETWORK_LATENCY` | 100e-3 | 100 milliseconds |
| `MAX_CLOGGING_LATENCY` | 0 | Per-connection clogging (buggified: 0.1s) |
| `MAX_BUGGIFIED_DELAY` | 0 | General buggified delay (buggified: 0.2s) |
| `MAX_CLOGGING_DELAY` | 0 | Receiver actor jitter |
| `CONNECTION_FAILURE_DELAY` | -- | Delay before connect throws on failed process |
| `SIM_CONNECT_DELAY` | -- | Random delay during connection setup |
