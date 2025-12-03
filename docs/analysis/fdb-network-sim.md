# FoundationDB network simulation: a complete implementation guide

FoundationDB's deterministic simulation framework is the cornerstone of its legendary reliability, having accumulated **one trillion CPU-hours of simulated stress testing**. This system enables complete reproducibility of distributed system failures through a single random seed, allowing developers to debug complex race conditions and network failures with perfect repeatability. The architecture cleanly separates real and simulated networking through the `INetwork` abstraction, enabling identical application code to run in production with real TCP or in simulation with virtual time and injected faults.

## Architecture of the network abstraction layer

The foundation of FDB's simulation capability rests on a carefully designed abstraction hierarchy that isolates all sources of non-determinism behind interfaces. The key insight is that distributed systems bugs are often timing-dependent—by controlling time itself, FDB can reproduce any failure scenario.

```
┌─────────────────────────────────────────────────────┐
│              Application Code (Actors)               │
├─────────────────────────────────────────────────────┤
│              FlowTransport Layer                     │
│    Endpoint addressing, Peer management, Messages    │
├─────────────────────────────────────────────────────┤
│         INetwork / IConnection / IListener           │
│              Core Abstractions                       │
├─────────────────────────────────────────────────────┤
│       Net2 (Production)  │  Sim2 (Simulation)       │
│       Boost.ASIO         │  Virtual Time + Memory   │
└─────────────────────────────────────────────────────┘
```

### The INetwork interface

`INetwork` is the central abstraction providing the event loop and all network services. Every implementation must provide these core methods:

```cpp
class INetwork {
public:
    // Event loop control
    virtual void run() = 0;                              // Main blocking event loop
    virtual void stop() = 0;                             // Signal termination
    
    // Time management - CRITICAL for determinism
    virtual double now() const = 0;                      // Current time (real or virtual)
    virtual double timer_monotonic() = 0;                // Monotonic clock
    
    // Async scheduling primitives
    virtual Future<Void> delay(double seconds, TaskPriority taskID) = 0;
    virtual Future<Void> yield(TaskPriority taskID) = 0;
    virtual void onMainThread(Promise<Void>&& signal, TaskPriority taskID) = 0;
    
    // The key abstraction check
    virtual bool isSimulated() const = 0;
    
    // Global state storage (for FlowTransport, metrics, etc.)
    virtual flowGlobalType global(int id) const = 0;
    virtual void setGlobal(size_t id, flowGlobalType v) = 0;
    
    // Network identity
    virtual NetworkAddress getLocalAddress() const = 0;
    virtual NetworkAddressList getLocalAddresses() const = 0;
    
    // DNS resolution
    virtual Future<std::vector<NetworkAddress>> resolveTCPEndpoint(
        const std::string& host, const std::string& service) = 0;
};
```

The global network instance is accessed via `g_network`, which is initialized differently for production versus simulation:

```cpp
// Production initialization
g_network = newNet2(tlsConfig, useThreadPool, useMetrics);

// Simulation initialization  
g_network = g_simulator = new Sim2(...);
```

### The IConnection interface

`IConnection` abstracts a single TCP connection (or its simulated equivalent):

```cpp
class IConnection : public ReferenceCounted<IConnection> {
public:
    // Lifecycle
    virtual void close() = 0;
    
    // Non-blocking I/O - returns bytes actually read/written
    virtual int read(uint8_t* begin, uint8_t* end) = 0;
    virtual int write(SendBuffer const* buffer, int limit) = 0;
    
    // Async notification - fires when I/O is possible
    virtual Future<Void> onReadable() = 0;
    virtual Future<Void> onWritable() = 0;
    
    // Connection metadata
    virtual NetworkAddress getPeerAddress() const = 0;
    virtual UID getDebugID() const = 0;
};
```

The `SendBuffer` structure supports scatter-gather I/O:

```cpp
struct SendBuffer {
    uint8_t* data;
    int bytes_written;   // How much has been sent
    int bytes_sent;      // Total bytes in buffer
    SendBuffer* next;    // Linked list for chaining
    
    int bytes_unsent() const { return bytes_sent - bytes_written; }
};
```

### NetworkAddress as peer lookup key

The `NetworkAddress` structure serves as the primary key for peer management:

```cpp
struct NetworkAddress {
    IPAddress ip;           // IPv4 or IPv6
    uint16_t port;
    uint16_t flags;         // FLAG_PRIVATE (1), FLAG_TLS (2)
    
    // Comparison operators for use as map key
    bool operator<(const NetworkAddress& other) const;
    bool operator==(const NetworkAddress& other) const;
    
    // String format: "192.168.1.1:4500" or "192.168.1.1:4500:tls"
    std::string toString() const;
    static NetworkAddress parse(const std::string& str);
};

struct NetworkAddressList {
    NetworkAddress address;                      // Primary
    Optional<NetworkAddress> secondaryAddress;   // For multi-homed processes
};
```

### Endpoint addressing for services

An `Endpoint` identifies a specific service/actor on a specific process, combining network address with a unique token:

```cpp
struct Endpoint {
    NetworkAddressList addresses;
    UID token;              // 128-bit unique identifier
    
    bool isLocal() const;   // Check if on current process
    NetworkAddress getPrimaryAddress() const;
    
    // Adjusted endpoints for multiplexing multiple services
    Endpoint getAdjustedEndpoint(uint32_t index) const;
};
```

## Transport wire protocol specification

FDB uses a length-prefixed framing protocol with **CRC32C checksums** for integrity verification.

### Packet structure

```
┌─────────────────────────────────────────────────────────────┐
│  uint32_t packetLength      │ Total bytes including header  │
├─────────────────────────────────────────────────────────────┤
│  uint32_t checksum          │ CRC32C of payload             │
├─────────────────────────────────────────────────────────────┤
│  UID token (16 bytes)       │ Destination endpoint          │
├─────────────────────────────────────────────────────────────┤
│  Serialized message payload (FlatBuffers)                   │
└─────────────────────────────────────────────────────────────┘
```

The **CRC32C** algorithm (Castagnoli polynomial) leverages hardware instructions on modern CPUs. Performance profiling shows `crc32c_append` is called from both `sendPacket` and `scanPackets` functions.

### Connection handshake (ConnectPacket)

```cpp
#pragma pack(push, 1)
struct ConnectPacket {
    uint32_t connectPacketLength;        // Size of this packet
    ProtocolVersion protocolVersion;     // 64-bit version (e.g., 0x0FDB00A400040001)
    uint16_t canonicalRemotePort;        // Sender's canonical port
    uint64_t connectionId;               // Unique ID for duplicate detection
    // Additional flags and metadata follow
};
#pragma pack(pop)
```

**Protocol version** format is a 64-bit value encoding major/minor/patch versions, used for compatibility checking between different FDB releases.

### Serialization integration

FDB uses **FlatBuffers** with a traits-based serialization system:

```cpp
// Type identification
constexpr static FileIdentifier file_identifier = 3152015;

// Traits for custom serialization
template <class T>
struct serializable_traits<ReplyPromise<T>> : std::true_type {
    template<class Archiver>
    static void serialize(Archiver& ar, ReplyPromise<T>& p) {
        if constexpr (Archiver::isDeserializing) {
            UID token;
            serializer(ar, token);
            auto endpoint = FlowTransport::transport().loadedEndpoint(token);
            p = ReplyPromise<T>(endpoint);
            networkSender(p.getFuture(), endpoint);  // Start reply handler
        } else {
            serializer(ar, p.getEndpoint().token);
        }
    }
};
```

## Net2 real network implementation

The `Net2` class implements `INetwork` using **Boost.ASIO** for platform-abstracted async I/O, automatically using epoll (Linux), kqueue (macOS/BSD), or IOCP (Windows).

### Run loop structure

```cpp
void Net2::run() {
    TraceEvent("Net2Running").log();
    thread_network = this;
    
    while (!stopped) {
        // 1. Process ready tasks from priority queue
        while (taskQueue.getNumReadyTasks() > 0) {
            double newTaskBegin = timer_monotonic();
            
            // Check for yield to prevent actor starvation
            if (check_yield(TaskPriority::Max, tscNow)) {
                checkForSlowTask(tscBegin, tscNow, duration, currentTaskID);
                ++countYields;
                break;
            }
            
            // Execute task
            currentTask.execute();
        }
        
        // 2. Run ASIO reactor to process network I/O
        reactor.ios.poll();
        
        // 3. Process timers
        processTimers();
    }
}
```

### Yield mechanism for actor starvation

The yield system uses **TSC (timestamp counter)** for high-resolution timing:

```cpp
bool Net2::check_yield(TaskPriority taskID, int64_t tscNow) {
    // Check if current task has exceeded its time slice
    // Returns true if the task should yield
}

void Net2::checkForSlowTask(int64_t tscBegin, int64_t tscEnd, 
                            double duration, TaskPriority priority) {
    double slowTaskProfilingLogInterval = 
        std::max(FLOW_KNOBS->RUN_LOOP_PROFILING_INTERVAL, 
                 FLOW_KNOBS->SLOWTASK_PROFILING_LOG_INTERVAL);
    if (duration > slowTaskProfilingLogInterval) {
        // Log slow task for debugging
        sampleRate = 1;  // Always include slow tasks
    }
}
```

### Time implementation

`now()` returns **real monotonic time** in production:

```cpp
double timer_monotonic();  // Returns seconds as double

// Performance note: switching clock source from xen to tsc
// improved read performance by 10-15%
```

### Connection lifecycle

```cpp
class Connection : public IConnection {
    boost::asio::ip::tcp::socket socket;
    
    ACTOR static Future<Reference<IConnection>> connect(
        boost::asio::io_service* ios, NetworkAddress addr) {
        state Reference<Connection> self(new Connection(*ios));
        // Async connect using Boost.ASIO
        wait(self->socket.async_connect(endpoint));
        return self;
    }
};

class SSLConnection : public IConnection {
    // Wraps Connection with SSL layer using boost::asio::ssl::stream
    ACTOR static Future<Reference<IConnection>> connect(
        boost::asio::io_service* ios,
        boost::asio::ssl::context* sslContext,
        NetworkAddress addr,
        tcp::socket* existingSocket);
};
```

## FlowTransport and peer management internals

`FlowTransport` is the singleton coordinator managing all peer-to-peer communication.

### FlowTransport structure

```cpp
class FlowTransport {
public:
    static FlowTransport& transport();  // Singleton
    
    // Connection management
    Future<Void> bind(NetworkAddress publicAddress, NetworkAddress listenAddress);
    Reference<Peer> getOrOpenPeer(NetworkAddress addr, bool createConnection = true);
    
    // Message sending
    void sendUnreliable(SerializeSource what, const Endpoint& destination, bool openConnection);
    
    // Endpoint registration
    void addWellKnownEndpoint(Endpoint& endpoint, NetworkMessageReceiver* receiver, TaskPriority taskID);
    Endpoint loadedEndpoint(const UID& token);
};
```

### Peer class and state machine

```cpp
struct Peer : NonCopyable {
    // Connection state
    Reference<IConnection> connection;
    NetworkAddress destination;
    
    // Message queues
    UnsentPacketQueue unsent;           // Outgoing packets
    ReliablePacketList reliable;        // Awaiting acknowledgment
    
    // Health monitoring
    double lastConnectTime;
    double lastDataPacketSentTime;
    Histogram<double> pingLatencies;
    int timeoutCount;
    
    // Connection identification
    UID connectionId;                   // For duplicate detection
    ProtocolVersion protocolVersion;
    bool compatible;
    
    // Managed actors
    Future<Void> connectionKeeper;      // Main lifecycle management
    Future<Void> connectionMonitor;     // Ping/pong health check
    Future<Void> connectionWriter;      // Async packet writer
    Future<Void> connectionReader;      // Async packet reader
};
```

**Peer state machine transitions:**

```
    [UNCONNECTED] ──connect()──▶ [CONNECTING]
         ▲                           │
         │                           │success
         │                           ▼
         │                    [HANDSHAKING]
         │                           │
         │                           │ConnectPacket exchanged
         │                           ▼
         │                    [ESTABLISHED]◄────┐
         │                           │          │(reconnect)
         │connection error/          │          │
         │timeout                    │ping timeout/
         │                           │connection closed
         │                           ▼          │
         └───────────────────[RECONNECTING]─────┘
                                     │
                               max retries exceeded
                                     ▼
                              [FAILED/REMOVED]
```

### Connection establishment and duplicate resolution

When two peers attempt simultaneous connections, the higher `connectionId` wins:

```cpp
// In connectionReader/incoming connection handler
if (pkt.connectionId > existingPeer->connectionId) {
    // Accept new connection, close old one
    existingPeer->connection->close();
    existingPeer->connection = newConnection;
    existingPeer->connectionId = pkt.connectionId;
} else {
    // Reject incoming, keep existing
    newConnection->close();
}
```

### Reconnection with exponential backoff

```cpp
// Knobs configuration
init(INITIAL_RECONNECTION_TIME,        0.05);   // 50ms initial
init(MAX_RECONNECTION_TIME,            0.5);    // 500ms maximum
init(RECONNECTION_TIME_GROWTH_RATE,    1.2);    // Exponential factor
init(RECONNECTION_RESET_TIME,          5.0);    // Reset after 5s success

ACTOR Future<Void> connectionKeeper(Reference<Peer> self, TransportData* transport) {
    state double reconnectionDelay = FLOW_KNOBS->INITIAL_RECONNECTION_TIME;
    
    loop {
        try {
            Reference<IConnection> conn = wait(
                INetworkConnections::net()->connect(self->destination));
            
            self->connection = conn;
            reconnectionDelay = FLOW_KNOBS->INITIAL_RECONNECTION_TIME;  // Reset
            
            wait(connectionWriter(self) || connectionReader(self, transport) || 
                 connectionMonitor(self));
                 
        } catch (Error& e) {
            if (e.code() == error_code_connection_failed) {
                wait(delay(reconnectionDelay));
                reconnectionDelay = std::min(
                    reconnectionDelay * FLOW_KNOBS->RECONNECTION_TIME_GROWTH_RATE,
                    FLOW_KNOBS->MAX_RECONNECTION_TIME);
            } else {
                throw;
            }
        }
    }
}
```

### Ping/pong health monitoring

```cpp
// Configuration
init(CONNECTION_MONITOR_LOOP_TIME,     isSimulated ? 0.75 : 1.0);  // Ping interval
init(CONNECTION_MONITOR_TIMEOUT,       isSimulated ? 1.50 : 2.0);  // Failure threshold
init(CONNECTION_MONITOR_IDLE_TIMEOUT,  180.0);                      // 3 minutes idle close

ACTOR Future<Void> connectionMonitor(Reference<Peer> peer) {
    loop {
        wait(delay(FLOW_KNOBS->CONNECTION_MONITOR_LOOP_TIME));
        
        if (!peer->connection) break;
        
        state double pingStart = now();
        try {
            wait(peer->connection->ping());
            double latency = now() - pingStart;
            peer->pingLatencies.addSample(latency);
        } catch (Error& e) {
            peer->timeoutCount++;
            if (peer->timeoutCount > threshold || 
                now() - peer->lastDataPacketSentTime > FLOW_KNOBS->CONNECTION_MONITOR_TIMEOUT) {
                throw connection_failed();
            }
        }
        
        // Close idle unreferenced connections
        if (now() - peer->lastDataPacketSentTime > FLOW_KNOBS->CONNECTION_MONITOR_IDLE_TIMEOUT &&
            peer->peerReferences == 0) {
            break;
        }
    }
}
```

## Sim2 deterministic simulation internals

The `Sim2` class is the heart of FDB's simulation testing, implementing `INetwork` with **virtual time** and **memory-based connections**.

### Virtual time representation

Time is represented as a `double` (seconds) and advances **discretely** via the event loop:

```cpp
class Sim2 : public ISimulator, public INetwork, public INetworkConnections {
    double currentTime;  // Virtual time
    
    double now() const override {
        return currentTime;  // NOT real time!
    }
    
    bool isSimulated() const override { return true; }
};
```

**Time compression:** Simulations typically achieve **10:1 real-to-simulated time ratio**. When CPU is idle, the simulator fast-forwards to the next scheduled event.

### Event queue and time advancement

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

**Event ordering at same timestamp:**
1. **Time** is the primary sort key
2. **TaskPriority** determines order within the same time
3. **Insertion order** (FIFO) for same priority

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

**Time advancement is blocked** when:
- Ready queue has tasks at current time
- Coroutine is executing (time frozen during execution)

### Sim2Conn implementation

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

### Receiver actor - the core transfer mechanism

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
        
        // Randomly decide how many bytes to "receive" (partial delivery simulation)
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

### Simulated connection establishment

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
    
    // Set random latency for this connection
    auto latency = g_clogging.setPairLatencyIfNotSet(
        peerProcess->address.ip, process->address.ip,
        FLOW_KNOBS->MAX_CLOGGING_LATENCY * deterministicRandom()->random01());
    
    // Simulate connection delay
    wait(delay(deterministicRandom()->random01() * FLOW_KNOBS->SIM_CONNECT_DELAY));
    
    // Register with listener (simulates accept())
    g_simulator->connections[toAddr].send({g_network->getLocalAddress(), peer});
    
    // Start receiver actors
    self->receiverActor = receiver(self.getPtr());
    peer->receiverActor = receiver(peer.getPtr());
    
    return self;
}
```

### Latency configuration knobs

```cpp
// From Knobs.cpp
init(MIN_NETWORK_LATENCY,           100e-6);   // 100 microseconds
init(FAST_NETWORK_LATENCY,          800e-6);   // 800 microseconds  
init(SLOW_NETWORK_LATENCY,          100e-3);   // 100 milliseconds
init(MAX_CLOGGING_LATENCY,          0);        // Buggified: up to 0.1s
init(MAX_BUGGIFIED_DELAY,           0);        // Buggified: up to 0.2s
```

### Connection close behavior

When `close()` is called on a `Sim2Conn`:

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
- **Unsent data**: Discarded from sender's buffer
- **In-transit data**: Lost (simulating real network)
- **Received but unread**: Remains briefly, then `connection_failed` raised
- **Outstanding futures**: Cancelled with `connection_failed`

## Network topology and partition modeling

### Simulated cluster hierarchy

```
Cluster
├── Datacenter 1
│   ├── Machine 1
│   │   ├── Process 1 (StorageServer)
│   │   └── Process 2 (TLog)
│   └── Machine 2
│       └── ...
├── Datacenter 2
│   └── ...
└── Satellites
```

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

### Network partition types

- **Machine-level**: Single machine isolated
- **Rack-level**: Entire rack partitioned
- **Datacenter-level**: Full DC partitioned
- **Asymmetric partitions**: A → B works, B → A fails

```cpp
void clogTlog(double seconds) {
    for (const auto& process : g_simulator->getAllProcesses()) {
        // g_simulator methods:
        // - clogInterface() - clog specific interface
        // - clogPair() - clog connection between addresses
    }
}
```

## Deterministic random and BUGGIFY system

### DeterministicRandom implementation

FDB uses **std::mt19937_64** (64-bit Mersenne Twister):

```cpp
class DeterministicRandom : public IRandom {
    std::mt19937_64 random;
    
public:
    DeterministicRandom(uint32_t seed) : random(seed) {}
    
    double random01();                        // [0, 1)
    int randomInt(int min, int max);          // [min, max)
    int64_t randomInt64(int64_t min, int64_t max);
    uint32_t randomUInt32();
    UID randomUniqueID();
    uint32_t randomSkewedUInt32(uint32_t min, uint32_t max);
};
```

### Global access pattern

```cpp
// Deterministic (reproducible with seed)
Reference<IRandom> deterministicRandom();

// Non-deterministic (for production UIDs, etc.)
Reference<IRandom> nondeterministicRandom();

// Initialization
int main() {
    int randomSeed = platform::getRandomSeed();
    g_random = new DeterministicRandom(randomSeed);
    g_nondeterministic_random = new DeterministicRandom(platform::getRandomSeed());
}
```

### BUGGIFY macro system

**Core rules:**
1. BUGGIFY only evaluates to true **in simulation**
2. First evaluation **enables/disables for entire run** (deterministic)
3. Enabled uses have **25% chance** of being true

```cpp
// Basic BUGGIFY
if (BUGGIFY) {
    // Inject fault - reproducible with same seed
}

// Custom probability
if (BUGGIFY_WITH_PROB(0.001)) {
    throw disk_adapter_reset();
}
```

### BUGGIFY patterns in transport layer

| Location | Fault Injected | Purpose |
|----------|---------------|---------|
| Connection timing | `CONNECTION_MONITOR_LOOP_TIME = 6.0` | Test slow ping intervals |
| Connection timeout | `CONNECTION_MONITOR_TIMEOUT = 6.0` | Test delayed failure detection |
| Packet bit flipping | Random bit flips | Test checksum validation |
| Connection delays | Artificial delays | Test timeout handling |

```cpp
// Bit flipping for checksum testing
if (g_network->isSimulated() && BUGGIFY_WITH_PROB(0.0001)) {
    g_simulator.lastConnectionFailure = g_network->now();
    int flipBits = 32 - (int)floor(log2(deterministicRandom()->randomUInt32()));
    // Flip bits to test error detection
}

// Knob randomization
init(DESIRED_TEAMS_PER_SERVER, 5);
if (randomize && BUGGIFY)
    DESIRED_TEAMS_PER_SERVER = deterministicRandom()->randomInt(1, 10);
```

### Seed-based reproduction

```bash
# Run with specific seed
./bin/fdbserver -r simulation -s 366751840 -f test.toml

# Verification: seed → unseed matching
# Same seed + same binary + same OS = identical unseed
```

**What breaks determinism:**
- Different OS types
- Different build environments (compiler, stdlib)
- Third-party code not in Flow
- Real time access, hash iteration order
- Multithreading

## Message flow trace comparison

### Real network path (Net2)

```
1. Application calls sendUnreliable(message, endpoint)
2. FlowTransport::sendUnreliable():
   - Lookup/create Peer for destination address
   - Serialize message with BinaryWriter
   - Add to Peer::unsent queue
3. connectionWriter actor:
   - Pop from unsent queue
   - Call connection->write() with SendBuffer
4. Net2::Connection::write():
   - Write to boost::asio socket
   - Returns bytes written
5. TCP stack transmits over real network
6. Remote Net2::Connection::read():
   - Boost.ASIO callback on data arrival
7. connectionReader actor:
   - Read from connection->read()
   - Call scanPackets() to parse
8. scanPackets():
   - Validate CRC32C checksum
   - Extract endpoint token (UID)
   - Lookup receiver in EndpointMap
   - Call receiver->receive(data)
```

### Simulated network path (Sim2)

```
1. Application calls sendUnreliable(message, endpoint)
2. FlowTransport::sendUnreliable():
   - Lookup/create Peer (same as real)
   - Serialize message (same as real)
   - Add to Peer::unsent queue (same as real)
3. connectionWriter actor:
   - Pop from unsent queue
   - Call Sim2Conn::write()
4. Sim2Conn::write():
   - Add bytes to unsent queue
   - Update sentBytes AsyncVar (triggers receiver)
   - Returns bytes "written" (immediate)
5. Sim2 receiver actor (on peer process):
   - Wait for sentBytes.onChange()
   - Wait random latency via delay()
   - Update receivedBytes (triggers onReadable)
6. connectionReader actor (same as real):
   - Wait for onReadable()
   - Call Sim2Conn::read() - returns from recvBuffer
   - Call scanPackets() - identical parsing
7. Endpoint dispatch - identical
```

**Key difference:** Steps 4-5 replace real TCP with deterministic, latency-injected memory transfer.

## Implementation patterns for Go

### Core interfaces

```go
type INetwork interface {
    Run()
    Stop()
    Now() float64
    Delay(seconds float64, priority TaskPriority) Future[struct{}]
    Yield(priority TaskPriority) Future[struct{}]
    IsSimulated() bool
    GetLocalAddress() NetworkAddress
}

type IConnection interface {
    Close()
    Read(buf []byte) (int, error)
    Write(buf []byte) (int, error)
    OnReadable() Future[struct{}]
    OnWritable() Future[struct{}]
    PeerAddress() NetworkAddress
}

type IListener interface {
    Close()
    Accept() Future[IConnection]
    ListenAddress() NetworkAddress
}
```

### Simulated connection

```go
type Sim2Conn struct {
    process     *ProcessInfo
    peerProcess *ProcessInfo
    peerConn    *Sim2Conn
    
    sentBytes     *AsyncVar[int64]
    receivedBytes *AsyncVar[int64]
    unsent        *Queue[[]byte]
    recvBuffer    bytes.Buffer
    
    closed bool
    id     UID
}

func (c *Sim2Conn) Write(buf []byte) (int, error) {
    if c.closed {
        return 0, ErrConnectionClosed
    }
    c.unsent.Push(buf)
    c.sentBytes.Set(c.sentBytes.Get() + int64(len(buf)))
    return len(buf), nil
}
```

### Event queue with virtual time

```go
type Sim2 struct {
    currentTime float64
    taskQueue   *PriorityQueue[Task]
    processes   map[NetworkAddress]*ProcessInfo
    rng         *DeterministicRandom
}

func (s *Sim2) Run() {
    for !s.stopped && s.taskQueue.Len() > 0 {
        task := s.taskQueue.Pop()
        s.currentTime = task.Time  // Advance virtual time
        s.switchToProcess(task.Process)
        task.Action()
    }
}

func (s *Sim2) Delay(seconds float64, priority TaskPriority) Future[struct{}] {
    p := NewPromise[struct{}]()
    s.taskQueue.Push(Task{
        Time:     s.currentTime + seconds,
        Priority: priority,
        Action:   func() { p.Send(struct{}{}) },
    })
    return p.Future()
}
```

## Implementation checklist

Building a similar system requires implementing these components in order:

1. **DeterministicRandom** - Mersenne Twister with seed management
2. **Future/Promise primitives** - Async primitives with deterministic scheduling
3. **TaskPriority enum** - Priority levels for event ordering
4. **NetworkAddress/Endpoint** - Addressing types with comparison operators
5. **IConnection interface** - Abstract connection with read/write/notify
6. **IListener interface** - Abstract accept socket
7. **INetwork interface** - Core abstraction with time, delay, yield
8. **Priority queue** - Min-heap ordered by (time, priority)
9. **Sim2 event loop** - Virtual time advancement, process switching
10. **Sim2Conn** - Memory-based connection with latency injection
11. **Receiver actor** - Async byte transfer with random delays
12. **Net2 (optional)** - Real network using Go's net package
13. **FlowTransport** - Peer management, endpoint dispatch
14. **Peer state machine** - Connection lifecycle with reconnection
15. **Wire protocol** - Length-prefixed framing with CRC32C
16. **BUGGIFY system** - Deterministic fault injection macros
17. **TraceEvent logging** - Structured event logging for debugging

## Conclusion

FoundationDB's deterministic simulation represents one of the most sophisticated testing frameworks ever built for distributed systems. The key architectural insights are:

**Clean abstraction boundaries** through `INetwork`/`IConnection` interfaces allow identical application code to run against real or simulated networks. The abstraction is nearly perfect—only `isSimulated()` checks leak through, and even those are primarily for BUGGIFY fault injection.

**Virtual time** enables compressing hours of simulated execution into minutes of real time, while ensuring perfect reproducibility. The min-priority queue event loop with (time, priority) ordering guarantees deterministic execution order.

**Sim2Conn's receiver actor** elegantly simulates network behavior using `AsyncVar` counters and `delay()` calls, injecting configurable latency without any real I/O.

**BUGGIFY** transforms fault injection from manual test case creation into automatic exploration of the failure space, with the critical property that each injection point's enabled/disabled state is fixed per-run based on the seed.

Reimplementing this system in Go would leverage goroutines for actors, channels for futures, and Go's built-in `net` package (which already abstracts epoll/kqueue) for real networking. The most critical component is the deterministic event loop with virtual time—everything else builds on that foundation.