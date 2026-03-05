# Layer 1: Flow Runtime (`flow/`)

## Layer Position

```
┌─────────────────────────────────────────────────────┐
│              Application Code (Actors)               │
├─────────────────────────────────────────────────────┤
│         Layer 2: FlowTransport (fdbrpc/)             │
│    Endpoint addressing, Peer management, Messages    │
├─────────────────────────────────────────────────────┤
│  >> Layer 1: Flow Runtime (flow/) <<                 │
│    INetwork / IConnection / IListener                │
│    Future/Promise/SAV, TaskPriority, BUGGIFY         │
│    DeterministicRandom, Event Loop                   │
├─────────────────────────────────────────────────────┤
│       Net2 (Production)  │  Sim2 (Simulation)        │
│       Boost.ASIO         │  Virtual Time + Memory    │
└─────────────────────────────────────────────────────┘
```

Everything in `flow/` is the runtime substrate. It provides the actor execution model, async primitives, network abstractions, time, randomness, and fault injection. Upper layers (FlowTransport, application actors) depend on it but never on a specific implementation (Net2 vs Sim2).

---

## Flow Language

Flow is a custom transpiler that adds actor-based concurrency to C++11. It processes `.actor.cpp` files and generates standard C++ with callbacks, enabling deterministic simulation of distributed systems.

**Source**: `flow/include/flow/flow.h`, `flow/actorcompiler/`

### ACTOR keyword

Marks asynchronous functions returning `Future<T>`. Compiled to state machines.

```cpp
ACTOR Future<int> asyncFunction(Parameters...) {
    // Actor body compiled to state machine
}
```

### wait() statement

Pauses actor execution until a Future completes. Only usable inside ACTOR functions. Non-blocking -- other actors continue. Becomes a yield point in the generated state machine.

### state keyword

Declares variables that persist across `wait()` boundaries:

```cpp
ACTOR Future<Void> example() {
    state int counter = 0;  // Survives across wait()
    wait(delay(1.0));
    counter++;              // Still accessible
}
```

### choose/when construct

Waits for multiple futures simultaneously (select-style):

```cpp
choose {
    when(T value = wait(future1)) { /* handle future1 */ }
    when(U value = wait(future2)) { /* handle future2 */ }
}
```

Common pattern -- timeout:

```cpp
ACTOR Future<Optional<T>> withTimeout(Future<T> f, double timeout) {
    choose {
        when(T value = wait(f)) { return Optional<T>(value); }
        when(wait(delay(timeout))) { return Optional<T>(); }
    }
}
```

### loop statement

Creates infinite loops in actors. Combined with `wait()` for retry patterns.

---

## Task Priority System

**Source**: `flow/include/flow/flow.h`

Controls execution order within the single-threaded event loop. Higher value = higher priority.

```cpp
enum class TaskPriority {
    Max                = 1000000,
    RunLoop            = 30000,
    ASIOReactor        = 20001,
    RunCycleFunction   = 20000,
    FlowLock           = 10001,
    Coordination       = 5000,
    Worker             = 4000,
    DefaultEndpoint    = 1000,
    UnknownEndpoint    = 500,
    Min                = 0
};
```

All actors run in a single thread. `g_network->run()` is the main event loop. Tasks are processed by priority order. No blocking operations allowed.

---

## Future / Promise / SAV Primitives

**Source**: `flow/include/flow/flow.h`

### SAV (Single Assignment Variable)

Core primitive underlying Promise/Future:
- Thread-safe single assignment
- Manages callback lists
- Triggers callbacks when value is set

### Promise/Future connection

```cpp
Promise<T> promise;
Future<T> future = promise.getFuture();
// SAV connects them internally
promise.send(value);  // Triggers future callbacks
```

**Ready queue management**: Callbacks are added to the ready queue when triggered, processed by priority in the main loop, ensuring deterministic execution order.

**Error propagation**: Errors automatically propagate through Future chains. Actor throws leads to Future containing error. `wait()` on an errored Future throws.

### Request/Response pattern

```cpp
ACTOR Future<Response> makeRequest(Request req) {
    state ReplyPromise<Response> promise;
    sendRequest(req, promise);
    Response resp = wait(promise.getFuture());
    return resp;
}
```

---

## Compilation (actorcompiler)

**Source**: `flow/actorcompiler/`

The actorcompiler processes `.actor.cpp` files and generates `.actor.g.cpp` files containing standard C++.

Each ACTOR function becomes a class with:
- State variables (from `state` declarations)
- Callback methods for each `wait()` point
- State machine logic driving transitions

Include order requirement:

```cpp
#include "flow/actorcompiler.h"  // Must be last include
```

---

## INetwork Interface

**Source**: `flow/include/flow/INetwork.h`

The central abstraction providing the event loop and all network services. The global instance `g_network` is swapped between implementations:

```cpp
// Production
g_network = newNet2(tlsConfig, useThreadPool, useMetrics);

// Simulation
g_network = g_simulator = new Sim2(...);
```

```cpp
class INetwork {
public:
    // Event loop control
    virtual void run() = 0;
    virtual void stop() = 0;

    // Time management -- CRITICAL for determinism
    virtual double now() const = 0;
    virtual double timer_monotonic() = 0;

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

---

## IConnection / IListener Interfaces

**Source**: `flow/include/flow/IConnection.h`

### IConnection

Abstracts a single TCP connection (or its simulated equivalent):

```cpp
class IConnection : public ReferenceCounted<IConnection> {
public:
    virtual void close() = 0;

    // Non-blocking I/O -- returns bytes actually read/written
    virtual int read(uint8_t* begin, uint8_t* end) = 0;
    virtual int write(SendBuffer const* buffer, int limit) = 0;

    // Async notification -- fires when I/O is possible
    virtual Future<Void> onReadable() = 0;
    virtual Future<Void> onWritable() = 0;

    // Connection metadata
    virtual NetworkAddress getPeerAddress() const = 0;
    virtual UID getDebugID() const = 0;
};
```

### SendBuffer

Supports scatter-gather I/O via a linked list:

```cpp
struct SendBuffer {
    uint8_t* data;
    int bytes_written;   // How much has been sent
    int bytes_sent;      // Total bytes in buffer
    SendBuffer* next;    // Linked list for chaining

    int bytes_unsent() const { return bytes_sent - bytes_written; }
};
```

### IListener

Abstracts a listening socket that accepts incoming connections:

```cpp
class IListener : public ReferenceCounted<IListener> {
public:
    virtual void close() = 0;
    virtual Future<Reference<IConnection>> accept() = 0;
    virtual NetworkAddress getListenAddress() const = 0;
};
```

### NetworkAddress

Primary key for peer management:

```cpp
struct NetworkAddress {
    IPAddress ip;           // IPv4 or IPv6
    uint16_t port;
    uint16_t flags;         // FLAG_PRIVATE (1), FLAG_TLS (2)

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

---

## Net2: Production Implementation

**Source**: `flow/Net2.actor.cpp`

Implements `INetwork` using Boost.ASIO for platform-abstracted async I/O (epoll on Linux, kqueue on macOS/BSD, IOCP on Windows).

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

### Yield mechanism

Uses TSC (timestamp counter) for high-resolution timing to prevent actor starvation:

```cpp
bool Net2::check_yield(TaskPriority taskID, int64_t tscNow) {
    // Returns true if the current task has exceeded its time slice
}

void Net2::checkForSlowTask(int64_t tscBegin, int64_t tscEnd,
                            double duration, TaskPriority priority) {
    double slowTaskProfilingLogInterval =
        std::max(FLOW_KNOBS->RUN_LOOP_PROFILING_INTERVAL,
                 FLOW_KNOBS->SLOWTASK_PROFILING_LOG_INTERVAL);
    if (duration > slowTaskProfilingLogInterval) {
        sampleRate = 1;  // Always include slow tasks in logs
    }
}
```

### Time

`now()` returns real monotonic time in production. Switching clock source from Xen to TSC improved read performance by 10-15%.

```cpp
double timer_monotonic();  // Returns seconds as double
```

### Connection lifecycle

```cpp
class Connection : public IConnection {
    boost::asio::ip::tcp::socket socket;

    ACTOR static Future<Reference<IConnection>> connect(
        boost::asio::io_service* ios, NetworkAddress addr) {
        state Reference<Connection> self(new Connection(*ios));
        wait(self->socket.async_connect(endpoint));
        return self;
    }
};

class SSLConnection : public IConnection {
    // Wraps Connection with SSL layer via boost::asio::ssl::stream
};
```

---

## DeterministicRandom

**Source**: `flow/include/flow/IRandom.h`

Uses `std::mt19937_64` (64-bit Mersenne Twister):

```cpp
class DeterministicRandom : public IRandom {
    std::mt19937_64 random;

public:
    DeterministicRandom(uint32_t seed) : random(seed) {}

    double random01();
    int randomInt(int min, int max);          // [min, max)
    int64_t randomInt64(int64_t min, int64_t max);
    uint32_t randomUInt32();
    UID randomUniqueID();
    uint32_t randomSkewedUInt32(uint32_t min, uint32_t max);
};
```

Two global instances:

```cpp
Reference<IRandom> deterministicRandom();      // Reproducible with seed
Reference<IRandom> nondeterministicRandom();    // For production UIDs, etc.
```

---

## BUGGIFY System

**Source**: `flow/include/flow/Buggify.h`

Deterministic fault injection for simulation. Three rules:

1. BUGGIFY only evaluates to true **in simulation**
2. First evaluation **enables/disables for the entire run** (deterministic per seed)
3. Enabled uses have **25% chance** of being true per evaluation

```cpp
if (BUGGIFY) {
    // Inject fault -- reproducible with same seed
}

if (BUGGIFY_WITH_PROB(0.001)) {
    throw disk_adapter_reset();
}
```

### Transport-layer BUGGIFY patterns

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
}

// Knob randomization
init(DESIRED_TEAMS_PER_SERVER, 5);
if (randomize && BUGGIFY)
    DESIRED_TEAMS_PER_SERVER = deterministicRandom()->randomInt(1, 10);
```
