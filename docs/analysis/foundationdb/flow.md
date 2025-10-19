```markdown
# Flow Language Summary for Code Analysis

- [https://apple.github.io/foundationdb/flow.html](https://apple.github.io/foundationdb/flow.html)
- [https://diabloneo.github.io/2021/02/18/FoundationDB-flow-part-1/](https://diabloneo.github.io/2021/02/18/FoundationDB-flow-part-1/)

## Reference Files
- `flow/flow.h` - Core Flow types and primitives
- `flow/actorcompiler.h` - Flow compiler integration
- `fdbrpc/FlowTransport.actor.cpp` - Network transport implementation
- `flow/Net2.actor.cpp` - Low-level network layer
- `fdbserver/fdbserver.actor.cpp` - Server main with actor examples
- `flow/Knobs.cpp` - Configuration parameters including network settings
- `fdbrpc/fdbrpc.h` - RPC layer definitions
- `fdbclient/NativeAPI.actor.cpp` - Client API implementation
- Files ending in `.actor.cpp` - Contain Flow actor code
- Files ending in `.actor.g.cpp` - Generated C++ from Flow compiler

## What is Flow?

Flow is a custom transpiler language built by FoundationDB that adds actor-based concurrency to C++11. It processes `.actor.cpp` files and generates standard C++ code with callbacks, enabling deterministic simulation of distributed systems.

## Core Architecture

### Deterministic Execution Model
- All actors run in a single thread
- `g_network->run()` is the main event loop
- Tasks are processed by priority order
- Random number generators use deterministic seeds
- No blocking operations allowed

### Task Priority System
Flow uses priorities to control execution order:
```cpp
enum class TaskPriority {
    Max = 1000000,
    RunLoop = 30000,
    ASIOReactor = 20001,
    RunCycleFunction = 20000,
    FlowLock = 10001,
    Coordination = 5000,
    Worker = 4000,
    DefaultEndpoint = 1000,
    UnknownEndpoint = 500,
    Min = 0
};
```

## Key Language Features

### 1. ACTOR Keyword
Marks asynchronous functions that return `Future<T>`
```cpp
ACTOR Future<int> asyncFunction(Parameters...) {
    // Actor body compiled to state machine
}
```

### 2. wait() Statement
- Pauses actor execution until a Future completes
- Only usable inside ACTOR functions
- Non-blocking - other actors continue execution
- Becomes a yield point in generated state machine

### 3. state Keyword
Declares variables that persist across wait() statements
```cpp
ACTOR Future<Void> example() {
    state int counter = 0;  // Survives across wait()
    wait(delay(1.0));
    counter++;  // Still accessible
}
```

### 4. choose/when Construct
Waits for multiple futures simultaneously
```cpp
choose {
    when(T value = wait(future1)) { /* handle future1 */ }
    when(U value = wait(future2)) { /* handle future2 */ }
}
```

### 5. loop Statement
Creates infinite loops in actors

## Promise/Future Implementation

### SAV (Single Assignment Variable)
- Core primitive underlying Promise/Future
- Thread-safe single assignment
- Manages callback lists
- Triggers callbacks when value is set

### Promise/Future Connection
```cpp
Promise<T> promise;
Future<T> future = promise.getFuture();
// SAV connects them internally
promise.send(value);  // Triggers future callbacks
```

### Ready Queue Management
- Callbacks added to ready queue when triggered
- Processed by priority in main loop
- Ensures deterministic execution order

## Compilation Process

### actorcompiler Tool
- Processes `.actor.cpp` files
- Generates state machines from ACTOR functions
- Each `wait()` becomes a state transition
- Outputs `.actor.g.cpp` files with generated C++

### Generated Structure
- ACTOR functions become classes with:
  - State variables
  - Callback methods for each wait point
  - State machine logic

### Include Order
```cpp
#include "flow/actorcompiler.h"  // Must be last include
```

## Network Integration

### Network Initialization
```cpp
g_network = newNet2(NetworkAddress(), false);
// Set up network options
g_network->run();  // Main event loop
```

### Network Stack
- Net2: Implements INetwork interface
- Uses boost::asio for async I/O
- All I/O operations return Futures
- Integrates with Flow's task priority system

## Simulation Mode

### Architecture
- `g_pSimulator` replaces `g_network` in simulation
- Controls time advancement
- Simulates network failures and delays
- Provides deterministic execution

### Time Control
```cpp
// In simulation
wait(delay(5.0));  // Can advance 5 seconds instantly
// Time is controlled, not real
```

### Key Features
- Single-threaded execution of entire cluster
- Reproducible with same random seed
- Can inject failures at any point
- Tests years of runtime in minutes

## Common Flow Patterns

### Timeout Pattern
```cpp
ACTOR Future<Optional<T>> withTimeout(Future<T> f, double timeout) {
    choose {
        when(T value = wait(f)) { return Optional<T>(value); }
        when(wait(delay(timeout))) { return Optional<T>(); }
    }
}
```

### Error Propagation
- Errors automatically propagate through Future chains
- Actor throws → Future contains error
- `wait()` on errored Future throws

### Request/Response Pattern
```cpp
ACTOR Future<Response> makeRequest(Request req) {
    state ReplyPromise<Response> promise;
    sendRequest(req, promise);
    Response resp = wait(promise.getFuture());
    return resp;
}
```

---

# Chinese Article Summary: FoundationDB Network Implementation

## Reference Files
- `fdbrpc/FlowTransport.actor.cpp` - Main transport layer implementation
- `fdbrpc/FlowTransport.h` - Transport layer interfaces and Peer class
- `flow/Net2.actor.cpp` - Network implementation with Connection class
- `fdbrpc/fdbrpc.h` - RPC interfaces and endpoint definitions
- `fdbrpc/NetworkSender.actor.h` - Network sending actor
- `fdbserver/ClusterController.actor.cpp` - Connection monitoring examples
- `flow/Knobs.cpp` - Network timeouts and parameters
- `fdbserver/workloads/Ping.actor.cpp` - Network testing workload
- Blog: "FoundationDB flow -- Part 1" (diabloneo.github.io)
- Blog: "FoundationDB flow -- Part 2" (diabloneo.github.io)
- GitHub issues: #2463, #1640 - FlowTransport discussions

## Flow Basics (from Part 1)

### Core Concepts
- **Single-threaded execution**: All Flow code runs in one thread
- **Task priority system**: Controls execution order deterministically
- **SAV (Single Assignment Variable)**: Core primitive for Promise/Future
- **Main loop**: `g_network->run()` processes ready tasks by priority

### Network Initialization
```cpp
g_network = newNet2(NetworkAddress(), false);
setupNetwork();
runNetwork();  // Calls g_network->run()
```

### Task Scheduling
- Tasks added to ready queue when futures complete
- Processed in priority order
- Higher priority tasks preempt lower ones
- Ensures deterministic execution