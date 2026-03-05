# FoundationDB's Actor Model and Endpoint Messaging System: A Technical Implementation Guide

FoundationDB implements a sophisticated actor-based RPC system where **endpoints are first-class network addresses**, **promises transparently resolve across network boundaries**, and **interfaces eliminate service discovery by embedding all necessary endpoint information**. The architecture centers on three core innovations: 128-bit endpoint tokens that uniquely identify message receivers, a serialization trait system that automatically spawns network sender actors when deserializing reply promises, and a unified programming model where the same Promise/Future semantics work identically for local and remote communication.

---

## Architecture overview: how the pieces connect

The system operates through layered abstractions. At the base, **FlowTransport** manages TCP connections and routes packets to endpoints. Each endpoint is identified by a **UID token** (128-bit) plus a **network address**. When a message arrives, FlowTransport extracts the token from the packet header, looks up the corresponding receiver in the **EndpointMap**, and invokes its `receive()` method with a deserializer.

The key insight is that **type information travels with the endpoint**, not with the message. Each `RequestStream<T>` creates a `NetNotifiedQueue<T>` that knows exactly what type to deserialize because the template parameter `T` is fixed at compile time. When you register a `RequestStream<GetValueRequest>`, the corresponding `NetNotifiedQueue<GetValueRequest>` will always deserialize incoming bytes as `GetValueRequest`.

```
┌──────────────────────────────────────────────────────────────────────────┐
│                          Application Layer                                │
│  ┌─────────────────┐      ┌─────────────────┐     ┌─────────────────┐    │
│  │  Interface      │      │  RequestStream  │     │  ReplyPromise   │    │
│  │  (bundles RPCs) │ ───► │  <T>            │ ◄── │  <T>            │    │
│  └─────────────────┘      └────────┬────────┘     └────────┬────────┘    │
│                                    │                       │             │
├────────────────────────────────────┼───────────────────────┼─────────────┤
│                          RPC Layer │                       │             │
│                      ┌─────────────▼───────┐   ┌───────────▼──────┐     │
│                      │  NetNotifiedQueue   │   │  networkSender   │     │
│                      │  <T>                │   │  ACTOR           │     │
│                      │  (FlowReceiver)     │   │  (spawned on     │     │
│                      └─────────────────────┘   │   deserialize)   │     │
│                                                └──────────────────┘     │
├──────────────────────────────────────────────────────────────────────────┤
│                        Transport Layer                                    │
│  ┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐      │
│  │  FlowTransport  │    │  EndpointMap    │    │  Peer           │      │
│  │  (singleton)    │───►│  (token→recv)   │    │  (per address)  │      │
│  └─────────────────┘    └─────────────────┘    └─────────────────┘      │
└──────────────────────────────────────────────────────────────────────────┘
```

---

## Core data structures you must implement

### UID: the 128-bit endpoint identifier

The `UID` struct forms the foundation of endpoint addressing. Its simplicity belies its importance—every endpoint, every actor, and every message is ultimately addressed via UIDs.

```cpp
struct UID {
    uint64_t part[2];  // 128 bits total
    
    UID() : part{0, 0} {}
    UID(uint64_t a, uint64_t b) : part{a, b} {}
    
    // Generation: cryptographically random 128-bit value
    static UID randomUID() {
        return UID(
            deterministicRandom()->randomUInt64(),
            deterministicRandom()->randomUInt64()
        );
    }
    
    // String representation: 32 hex characters
    std::string toString() const {
        return fmt::format("{:016x}{:016x}", part[0], part[1]);
    }
    
    // Comparison for hash map usage
    bool operator==(const UID& other) const {
        return part[0] == other.part[0] && part[1] == other.part[1];
    }
    
    // Serialization: 16 bytes, two consecutive uint64_t
    template<class Ar>
    void serialize(Ar& ar) {
        serializer(ar, part[0], part[1]);
    }
};
```

**Well-known UIDs** use a special pattern: `UID(-1, ++tokenCounter)`. The first part being `-1` (all 1s in binary) distinguishes them from random UIDs, while the counter ensures uniqueness across system services.

### Endpoint: combining address and identity

An `Endpoint` pairs a network location with a unique token. This combination allows direct addressing without service discovery.

```cpp
struct NetworkAddress {
    IPAddress ip;      // IPv4 (4 bytes) or IPv6 (16 bytes)
    uint16_t port;
    uint16_t flags;    // FLAG_TLS, FLAG_PUBLIC, etc.
    
    template<class Ar>
    void serialize(Ar& ar) {
        serializer(ar, ip, port, flags);
    }
};

struct NetworkAddressList {
    NetworkAddress address;                    // Primary address
    Optional<NetworkAddress> secondaryAddress; // For TLS+non-TLS dual-stack
    
    template<class Ar>
    void serialize(Ar& ar) {
        serializer(ar, address, secondaryAddress);
    }
};

struct Endpoint {
    NetworkAddressList addresses;
    UID token;
    
    // Create adjusted endpoint for interface serialization optimization
    Endpoint getAdjustedEndpoint(int index) const {
        Endpoint e = *this;
        e.token = UID(token.part[0] + index, token.part[1]);
        return e;
    }
    
    template<class Ar>
    void serialize(Ar& ar) {
        serializer(ar, addresses, token);
    }
};
```

### EndpointMap: routing tokens to receivers

The `EndpointMap` is a hash map that routes incoming messages to their destination actors. Well-known endpoints get special treatment with direct array indexing for performance.

```cpp
class EndpointMap {
private:
    std::unordered_map<UID, NetworkMessageReceiver*> dynamicEndpoints;
    NetworkMessageReceiver* wellKnownEndpoints[WLTOKEN_RESERVED_COUNT];
    
public:
    void insertWellKnown(NetworkMessageReceiver* receiver, UID token, TaskPriority priority) {
        // Well-known tokens use the second part as array index
        if (token.part[0] == (uint64_t)-1) {
            wellKnownEndpoints[token.part[1]] = receiver;
        } else {
            dynamicEndpoints[token] = receiver;
        }
    }
    
    NetworkMessageReceiver* get(UID token) {
        // Fast path for well-known endpoints
        if (token.part[0] == (uint64_t)-1 && token.part[1] < WLTOKEN_RESERVED_COUNT) {
            return wellKnownEndpoints[token.part[1]];
        }
        // Hash lookup for dynamic endpoints
        auto it = dynamicEndpoints.find(token);
        return it != dynamicEndpoints.end() ? it->second : nullptr;
    }
    
    void remove(UID token) {
        dynamicEndpoints.erase(token);
    }
};
```

---

## The RequestStream and NetNotifiedQueue pattern

`RequestStream<T>` is the primary interface for receiving typed messages. It wraps a `NetNotifiedQueue<T>*` that handles network deserialization and actor notification.

### NetNotifiedQueue: where type information lives

The critical insight is that **type `T` is baked into the queue at compile time**. When a packet arrives, the deserializer doesn't need to figure out what type to read—the `NetNotifiedQueue<T>` already knows.

```cpp
// Base class for network message receivers
struct NetworkMessageReceiver {
    virtual void receive(ArenaObjectReader& reader) = 0;
    virtual bool isStream() const = 0;
    virtual ~NetworkMessageReceiver() = default;
};

// FlowReceiver adds endpoint management
struct FlowReceiver : NetworkMessageReceiver {
    Endpoint endpoint;
    bool isLocalEndpoint;
    
    void makeWellKnownEndpoint(UID token, TaskPriority priority) {
        endpoint.token = token;
        endpoint.addresses = FlowTransport::transport().getLocalAddresses();
        FlowTransport::transport().addWellKnownEndpoint(endpoint, this, priority);
    }
};

// The typed queue that knows how to deserialize T
template<class T>
struct NetNotifiedQueue final : NotifiedQueue<T>, FlowReceiver {
    NetNotifiedQueue(int futures, int promises) 
        : NotifiedQueue<T>(futures, promises) {}
    
    void receive(ArenaObjectReader& reader) override {
        this->addPromiseRef();
        T message;
        reader.deserialize(message);  // TYPE T IS KNOWN HERE
        this->send(std::move(message));
        this->delPromiseRef();
    }
    
    bool isStream() const override { return true; }
};
```

### RequestStream: the application-facing API

```cpp
template<class T>
class RequestStream {
    NetNotifiedQueue<T>* queue;
    
public:
    RequestStream() : queue(new NetNotifiedQueue<T>(0, 1)) {}
    
    // Get the endpoint for serialization
    Endpoint getEndpoint() const {
        return queue->endpoint;
    }
    
    // Get a future that fires when messages arrive
    FutureStream<T> getFuture() const {
        return FutureStream<T>(queue);
    }
    
    // Send a message to this stream (local use)
    void send(T&& message) {
        queue->send(std::move(message));
    }
    
    // Get the underlying receiver for registration
    FlowReceiver* getReceiver(TaskPriority priority) {
        return queue;
    }
};
```

---

## ReplyPromise magic: the networkSender pattern

The most elegant part of fdbrpc is how `ReplyPromise<T>` transparently works across network boundaries. The secret lies in **different serialization behavior for sending vs. receiving**.

### The serializable_traits specialization

When a `ReplyPromise<T>` is **serialized** (on the client sending a request), only the endpoint token is written. When it's **deserialized** (on the server receiving the request), a `networkSender` actor is spawned that will send the reply back.

```cpp
template<class T>
struct serializable_traits<ReplyPromise<T>> : std::true_type {
    template<class Archiver>
    static void serialize(Archiver& ar, ReplyPromise<T>& p) {
        if constexpr (Archiver::isDeserializing) {
            // SERVER SIDE: Receiving request with embedded reply promise
            UID token;
            serializer(ar, token);
            
            // Reconstruct endpoint pointing back to client
            auto endpoint = FlowTransport::transport().loadedEndpoint(token);
            p = ReplyPromise<T>(endpoint);
            
            // THE MAGIC: Spawn actor that will send reply when promise is fulfilled
            networkSender(p.getFuture(), endpoint);
        } else {
            // CLIENT SIDE: Sending request, just serialize the token
            const auto& ep = p.getEndpoint().token;
            serializer(ar, ep);
        }
    }
};
```

### The networkSender actor

This actor waits for the local promise to be fulfilled, then sends the result over the network to the original requester:

```cpp
ACTOR template<class T>
void networkSender(Future<T> input, Endpoint endpoint) {
    try {
        T value = wait(input);  // Wait for promise to be fulfilled
        
        // Send success response back to client
        FlowTransport::transport().sendUnreliable(
            SerializeSource<ErrorOr<T>>(value),
            endpoint,
            false  // not a reply to a reply
        );
    } catch (Error& err) {
        // Send error response back to client
        FlowTransport::transport().sendUnreliable(
            SerializeSource<ErrorOr<T>>(err),
            endpoint,
            false
        );
    }
}
```

### Complete RPC message flow

Here's the step-by-step flow of an RPC call:

```
CLIENT                                        SERVER
------                                        ------
1. Create request with ReplyPromise
   ┌─────────────────────────┐
   │ GetValueRequest {       │
   │   key: "mykey"          │
   │   reply: ReplyPromise   │  ◄── Has endpoint pointing to client
   │ }                       │
   └─────────────────────────┘

2. Serialize request
   - key serialized normally
   - ReplyPromise serializes ONLY its token (16 bytes)

3. Send to server endpoint
   └──────────────────────────────────────────►

                                              4. Receive packet, extract UID
                                              5. Lookup receiver by token
                                              6. Deserialize into GetValueRequest
                                                 - ReplyPromise deserialization
                                                   spawns networkSender actor!
                                              
                                              7. Handler processes request:
                                                 req.reply.send(value);
                                                 
                                              8. networkSender actor wakes up,
                                                 sends result to client endpoint
   ◄──────────────────────────────────────────┘

9. Client receives response
10. Original future resolves with value
```

---

## Interface serialization: eliminating service discovery

FoundationDB interfaces bundle multiple `RequestStream<T>` endpoints into a single serializable struct. The key optimization is **endpoint adjustment**—only one base endpoint is serialized, and others are derived using offsets.

### Interface definition pattern

```cpp
struct StorageServerInterface {
    constexpr static FileIdentifier file_identifier = 15302073;
    
    UID uniqueID;
    LocalityData locality;
    
    RequestStream<GetValueRequest> getValue;
    RequestStream<GetKeyRequest> getKey;
    RequestStream<GetKeyValuesRequest> getKeyValues;
    RequestStream<WatchValueRequest> watchValue;
    // ... more request streams
    
    // Initialize all endpoints at once
    void initEndpoints() {
        std::vector<std::pair<FlowReceiver*, TaskPriority>> streams;
        streams.push_back(getValue.getReceiver(TaskPriority::LoadBalancedEndpoint));
        streams.push_back(getKey.getReceiver(TaskPriority::LoadBalancedEndpoint));
        streams.push_back(getKeyValues.getReceiver(TaskPriority::LoadBalancedEndpoint));
        streams.push_back(watchValue.getReceiver(TaskPriority::LoadBalancedEndpoint));
        FlowTransport::transport().addEndpoints(streams);
    }
    
    template<class Ar>
    void serialize(Ar& ar) {
        serializer(ar, uniqueID, locality, getValue);
        
        // OPTIMIZATION: Derive other endpoints from getValue's endpoint
        if constexpr (Ar::isDeserializing) {
            getKey = RequestStream<GetKeyRequest>(
                getValue.getEndpoint().getAdjustedEndpoint(1));
            getKeyValues = RequestStream<GetKeyValuesRequest>(
                getValue.getEndpoint().getAdjustedEndpoint(2));
            watchValue = RequestStream<WatchValueRequest>(
                getValue.getEndpoint().getAdjustedEndpoint(10));
        }
    }
};
```

### How endpoint adjustment works

The `getAdjustedEndpoint(int offset)` method creates a new endpoint by adding `offset` to the first part of the UID:

```cpp
// Base endpoint token: (0x123456789ABCDEF0, 0xFEDCBA9876543210)
// getAdjustedEndpoint(1):  (0x123456789ABCDEF1, 0xFEDCBA9876543210)
// getAdjustedEndpoint(2):  (0x123456789ABCDEF2, 0xFEDCBA9876543210)
```

This means you only serialize one endpoint, saving **14 bytes per additional RequestStream** in the interface.

---

## Bootstrap mechanism and well-known endpoints

The system bootstraps from a **cluster file** containing coordinator addresses, uses **well-known endpoint tokens** to contact them, and discovers all other services through the **ClusterController**.

### Well-known endpoint tokens

```cpp
// Compile-time constants for system services
constexpr UID WLTOKEN_CLIENTLEADERREG_GETLEADER(-1, 2);
constexpr UID WLTOKEN_CLIENTLEADERREG_OPENDATABASE(-1, 3);
constexpr UID WLTOKEN_CLIENTLEADERREG_DESCRIPTOR_MUTABLE(-1, 4);
constexpr int WLTOKEN_RESERVED_COUNT = 64;  // Reserved for system use
```

These tokens are **hardcoded constants**—any client with the same version can compute them without discovery.

### Bootstrap sequence

```
1. READ CLUSTER FILE
   "description:ID@coordinator1:4500,coordinator2:4500,coordinator3:4500"
   
2. CONSTRUCT COORDINATOR ENDPOINTS
   For each coordinator address:
     Endpoint {
       addresses: [coordinator_ip:4500],
       token: WLTOKEN_CLIENTLEADERREG_GETLEADER  // Fixed constant!
     }

3. CONTACT COORDINATORS
   Send GetLeaderRequest to all coordinators
   Wait for majority agreement on cluster controller

4. GET CLUSTER CONTROLLER INTERFACE
   LeaderInfo response contains ClusterControllerInterface
   
5. OPEN DATABASE
   Contact ClusterController via its interface
   Receive CommitProxyInterface, GrvProxyInterface, etc.
   
6. READY FOR OPERATIONS
   Client can now perform transactions using proxy interfaces
```

### ServerDBInfo: cluster-wide interface broadcast

Rather than querying for every interface, the ClusterController **broadcasts** a `ServerDBInfo` struct containing all critical interfaces to every worker in the cluster:

```cpp
struct ServerDBInfo {
    UID id;
    ClusterControllerInterface clusterInterface;
    MasterInterface master;
    vector<CommitProxyInterface> commitProxies;
    vector<GrvProxyInterface> grvProxies;
    vector<ResolverInterface> resolvers;
    // ... more interfaces
};
```

Workers subscribe to updates and always have current interface information without explicit discovery.

---

## FlowTransport internals: connections and routing

### Peer management

Each remote address gets a `Peer` object that manages the connection lifecycle:

```cpp
struct Peer : public ReferenceCounted<Peer> {
    NetworkAddress destination;
    Reference<IConnection> conn;
    Future<Void> connectionKeeper;  // Lifecycle management actor
    
    // Send buffers
    UnsentPacketQueue unsent;       // Fire-and-forget packets
    ReliablePacketList reliable;    // Packets requiring ACK
    
    // Metrics and state
    double lastConnectTime;
    bool connected;
    int64_t bytesReceived;
    int64_t bytesSent;
};
```

### Connection actors

Four actors manage each connection:

1. **connectionKeeper**: Manages connection lifecycle, reconnection attempts
2. **connectionWriter**: Drains send queues, writes to socket
3. **connectionReader**: Reads from socket, calls `scanPackets()`
4. **connectionMonitor**: Sends pings, detects failures

```cpp
ACTOR Future<Void> connectionKeeper(Reference<Peer> self) {
    loop {
        if (!self->conn) {
            // Attempt connection with timeout
            self->lastConnectTime = now();
            Reference<IConnection> conn = wait(timeout(
                INetworkConnections::net()->connect(self->destination),
                FLOW_KNOBS->CONNECTION_MONITOR_TIMEOUT,
                Reference<IConnection>()
            ));
            // ...
        }
        
        try {
            // Run connection actors in parallel
            wait(connectionWriter(self, conn) || 
                 connectionReader(self, conn) || 
                 connectionMonitor(self));
        } catch (Error& e) {
            if (e.code() == error_code_connection_failed) {
                // Wait FAILURE_DETECTION_DELAY before marking failed
                wait(delay(FLOW_KNOBS->FAILURE_DETECTION_DELAY));
            }
        }
    }
}
```

### Local vs. remote routing

When sending a message, FlowTransport checks if the destination is local:

```cpp
void sendMessage(Endpoint destination, SerializedMessage msg) {
    if (localAddresses.contains(destination.addresses.address)) {
        // LOCAL: Direct delivery, no serialization
        NetworkMessageReceiver* receiver = endpoints.get(destination.token);
        if (receiver) {
            receiver->receive(/* direct message */);
        }
    } else {
        // REMOTE: Queue for network transmission
        Reference<Peer> peer = getOrCreatePeer(destination.addresses.address);
        peer->unsent.push(msg);
    }
}
```

---

## Error handling and failure detection

### Broken promise detection

A `broken_promise` error occurs when all `Promise` objects are destroyed without fulfillment. The `networkSender` actor catches this and forwards the error to the remote caller:

```cpp
ACTOR template<class T>
void networkSender(Future<T> input, Endpoint endpoint) {
    try {
        T value = wait(input);
        // Send success
    } catch (Error& err) {
        // Including broken_promise - forward to remote
        FlowTransport::transport().sendUnreliable(
            SerializeSource<ErrorOr<T>>(err),
            endpoint,
            false
        );
    }
}
```

### FailureMonitor integration

The `FailureMonitor` tracks endpoint health cluster-wide:

- **4-second timeout**: Nodes that can't communicate with ClusterController for 4+ seconds are marked failed
- **100ms updates**: Workers receive failure status updates every 100ms
- **WaitFailure pattern**: Actors hold `ReplyPromise<Void>` without responding; if actor dies, callers get `broken_promise`

```cpp
// WaitFailure server pattern
ACTOR Future<Void> waitFailureServer(FutureStream<ReplyPromise<Void>> requests) {
    state std::vector<ReplyPromise<Void>> outstanding;
    loop {
        ReplyPromise<Void> req = waitNext(requests);
        outstanding.push_back(req);  // Hold without responding
        // If this actor dies, all promises break -> clients detect failure
    }
}
```

---

## Wire format details

### Packet structure

```
+--------+--------+------------------+----------------------+
| Length | Flags  | Endpoint Token   | FlatBuffers Payload  |
| 4 B    | varies | 16 B (UID)       | varies               |
+--------+--------+------------------+----------------------+
```

### Endpoint serialization

```
Endpoint (total: 20-40 bytes depending on IPv4/6 and secondary address):
┌────────────────────────────────────────────┐
│ NetworkAddressList                         │
│ ├── Primary: IPAddress(4/16) + Port(2) +  │
│ │            Flags(2) = 8-20 bytes         │
│ └── Secondary (Optional): same format      │
├────────────────────────────────────────────┤
│ UID Token                                  │
│ ├── part[0]: uint64_t (8 bytes)           │
│ └── part[1]: uint64_t (8 bytes)           │
└────────────────────────────────────────────┘
```

### FileIdentifier for type safety

Each serializable type has a `file_identifier` used for sanity checking:

```cpp
struct GetValueRequest {
    constexpr static FileIdentifier file_identifier = 8454530;
    // ...
};
```

The identifier appears at bytes 4-7 of the FlatBuffers payload and is checked during deserialization.

---

## Implementation checklist for recreating this system

If you're implementing a similar system in another language, build these components in order:

1. **UID type** with random generation and serialization
2. **NetworkAddress** with IPv4/IPv6 support
3. **Endpoint** combining address list and UID token
4. **Serialization framework** with bidirectional archiver (isDeserializing flag)
5. **Single-Assignment Variable (SAV)** for Promise/Future foundation
6. **Promise and Future** types with callback chains
7. **NotifiedQueue** for streaming multiple values
8. **NetNotifiedQueue** adding network receive capability
9. **RequestStream** wrapping NetNotifiedQueue
10. **ReplyPromise with serializable_traits** (the magic part)
11. **networkSender actor** spawned on deserialization
12. **EndpointMap** with hash map and well-known array
13. **FlowTransport** with local address tracking
14. **Peer** class with connection state
15. **Connection actors**: keeper, writer, reader, monitor
16. **Interface pattern** with endpoint adjustment
17. **Well-known endpoint constants** for bootstrap
18. **FailureMonitor** for cluster-wide health tracking

The most critical insight: **type information lives in the endpoint registration, not in the message format**. Each `RequestStream<T>` creates a receiver that knows type `T` at compile time. The network layer just routes bytes to the right receiver—the receiver knows what type to deserialize.

---

## Specific answers to your questions

**How does an actor obtain an endpoint that can receive messages?**
Create a `RequestStream<T>` which internally allocates a `NetNotifiedQueue<T>`. Call `initEndpoints()` on an interface or `makeWellKnownEndpoint()` for system services. The queue's `FlowReceiver` base class handles registration with FlowTransport's EndpointMap.

**How does RequestStream&lt;T&gt; know what type T to deserialize?**
The type `T` is a compile-time template parameter. When you create `RequestStream<GetValueRequest>`, the underlying `NetNotifiedQueue<GetValueRequest>::receive()` method calls `reader.deserialize<GetValueRequest>()`. The type is baked into the code at compile time, not transmitted on the wire.

**How does the system bootstrap?**
The cluster file provides coordinator addresses. Well-known tokens (`WLTOKEN_CLIENTLEADERREG_*`) are compile-time constants. Clients construct endpoints directly: `Endpoint{coordinator_address, WLTOKEN_GETLEADER}`. No discovery needed—the token is deterministic.

**What's the exact binary format of a serialized endpoint?**
`NetworkAddressList` (8-40 bytes: IP + port + flags, optionally repeated for secondary) followed by `UID` (16 bytes: two uint64_t). Total typically **24-56 bytes** depending on IPv4/IPv6 and dual-stack configuration.

**How are promises resolved across network boundaries?**
When `ReplyPromise<T>` is deserialized, `serializable_traits<ReplyPromise<T>>::serialize()` spawns a `networkSender` actor. This actor waits on the promise's future. When the server calls `reply.send(value)`, the local promise resolves, waking the networkSender, which serializes the value and sends it back to the client's endpoint (extracted from the original request's serialized token).