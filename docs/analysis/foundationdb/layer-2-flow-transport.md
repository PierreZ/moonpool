# Layer 2: FlowTransport

## Layer Position

```
┌──────────────────────────────────────────────────────┐
│  fdbrpc  (RequestStream, ReplyPromise, actor stubs)  │  Layer 3
├──────────────────────────────────────────────────────┤
│  FlowTransport  (peers, wire protocol, dispatch)     │  Layer 2  <-- this document
├──────────────────────────────────────────────────────┤
│  INetwork / IConnection  (Net2 or Sim2)              │  Layer 1
├──────────────────────────────────────────────────────┤
│  Flow runtime  (futures, actors, deterministic RNG)  │  Layer 0
└──────────────────────────────────────────────────────┘
```

FlowTransport is a "dumb pipe." It knows nothing about RPC semantics, actor types, or
application logic. Its job: maintain TCP connections to peers, frame messages on the wire,
and dispatch incoming bytes to the correct endpoint receiver by UID token lookup.

Source files (local copies in `docs/references/foundationdb/`, FoundationDB
`c0c44752`; see that directory's `README.md`):
- `fdbrpc/include/fdbrpc/FlowTransport.h`
- `fdbrpc/FlowTransport.cpp` (formerly `FlowTransport.actor.cpp`; now C++20 coroutines)
- `flow/include/flow/Net2Packet.h`

The code blocks below are simplified sketches. Where an earlier revision of this document
contradicted the source, the text has been corrected against `c0c44752`.

---

## 1. Core Data Structures

### UID -- 128-bit endpoint identifier

```cpp
struct UID {
    uint64_t part[2];  // 128 bits total

    static UID randomUID() {
        return UID(deterministicRandom()->randomUInt64(),
                   deterministicRandom()->randomUInt64());
    }

    template<class Ar>
    void serialize(Ar& ar) { serializer(ar, part[0], part[1]); }
};
```

**Well-known UIDs** use `UID(-1, ++tokenCounter)`. The `part[0] == (uint64_t)-1` sentinel
distinguishes them from random UIDs and enables array-indexed fast lookup.

### NetworkAddress / NetworkAddressList

```cpp
struct NetworkAddress {
    IPAddress ip;       // IPv4 or IPv6
    uint16_t port;
    uint16_t flags;     // FLAG_TLS, FLAG_PUBLIC, etc.
};

struct NetworkAddressList {
    NetworkAddress address;                     // Primary
    Optional<NetworkAddress> secondaryAddress;  // TLS+non-TLS dual-stack
};
```

### Endpoint -- address + token

```cpp
struct Endpoint {
    NetworkAddressList addresses;
    UID token;

    // Derive related endpoints by offsetting the token (FlowTransport.h:85)
    Endpoint getAdjustedEndpoint(uint32_t index) const {
        uint32_t newIndex = token.second();
        newIndex += index;
        return Endpoint(addresses,
            UID(token.first() + (uint64_t(index) << 32),
                (token.second() & 0xffffffff00000000LL) | newIndex));
    }
};
```

The adjustment moves both the high 32 bits of the first word and the low 32 bits (the
`EndpointMap` slot index) of the second word, so it only resolves because
`EndpointMap::insert(localAddresses, streams)` registers an interface's streams in
adjacent slots.

`getAdjustedEndpoint` allows a single `RequestStream` interface to expose multiple
sub-endpoints (e.g., request + reply) without extra registration.

---

## 2. EndpointMap

Routes incoming messages by token to the correct `NetworkMessageReceiver`.

```cpp
// FlowTransport.cpp:87
class EndpointMap : NonCopyable {
    struct Entry {
        union {
            uint64_t uid[2];   // priority packed into lower 32 bits; the token's
                               // lower 32 bits are the index in data[]
            uint32_t nextFree;
        };
        NetworkMessageReceiver* receiver = nullptr;
    };
    int wellKnownEndpointCount;
    std::vector<Entry> data;   // one indexed vector with a free list, no hash map
    uint32_t firstFree;
};
```

There is no hash map. The first `wellKnownEndpointCount` slots of `data` are reserved for
well-known tokens; dynamic tokens take a free slot, and the slot index is written into the
token's low 32 bits. `get(token)` indexes `data` directly and then checks the remaining
token bits against the stored entry, so a stale token for a reused slot is rejected.

---

## 3. FlowTransport Singleton

```cpp
class FlowTransport {
    static FlowTransport& transport();  // Process-global singleton

    Future<Void> bind(NetworkAddress publicAddress, NetworkAddress listenAddress);
    Reference<Peer> getOrOpenPeer(NetworkAddress addr, bool createConnection = true);
    void sendUnreliable(SerializeSource what, const Endpoint& destination, bool openConnection);
    void addWellKnownEndpoint(Endpoint& endpoint, NetworkMessageReceiver* receiver, TaskPriority taskID);
    Endpoint loadedEndpoint(const UID& token);
};
```

- `bind()` starts the listener; spawns a `listen()` actor that accepts connections in batches.
- `getOrOpenPeer()` returns existing peer or creates one, optionally initiating connection.
- `sendUnreliable()` is the primary send path -- serializes, queues into peer's unsent buffer.
- `addWellKnownEndpoint()` registers a receiver at a well-known token for the process lifetime.
- `loadedEndpoint()` reconstructs an Endpoint from a deserialized token (used during `ReplyPromise` deserialization).

---

## 4. Peer Management

### Peer struct

```cpp
struct Peer : NonCopyable {
    Reference<IConnection> connection;
    NetworkAddress destination;

    // Message queues
    UnsentPacketQueue unsent;
    ReliablePacketList reliable;

    // Health state
    double lastConnectTime;
    double lastDataPacketSentTime;
    int timeoutCount;
    Histogram<double> pingLatencies;

    // Identity
    UID connectionId;               // Duplicate connection resolution
    ProtocolVersion protocolVersion;
    bool compatible;

    // Managed actors
    Future<Void> connectionKeeper;
    Future<Void> connectionReader;
    Future<Void> connectionWriter;
    Future<Void> connectionMonitor;
};
```

### State machine

```
[UNCONNECTED] ──connect()──> [CONNECTING]
      ^                            |
      |                        success
      |                            v
      |                     [HANDSHAKING]
      |                            |
      |                   ConnectPacket exchanged
      |                            v
      |                     [ESTABLISHED] <------+
      |                            |             | (reconnect)
      | connection error/          |             |
      | timeout                    | ping timeout/
      |                            | conn closed |
      |                            v             |
      +----------------------[RECONNECTING]------+
                                   |
                            max retries exceeded
                                   v
                            [FAILED/REMOVED]
```

### Duplicate connection resolution

`connectionId` does **not** decide simultaneous connections (it only pairs a multi-version
client's two connections). `Peer::onIncomingConnection` (`FlowTransport.cpp:1116`) keeps the
incoming connection when the remote is not public, when our outgoing connection is idle,
when the remote's canonical address is larger than ours, or when our last connect attempt
is older than `ALWAYS_ACCEPT_DELAY`; otherwise it closes the incoming one and keeps its own:

```cpp
if (!destination.isPublic() || outgoingConnectionIdle || destination > compatibleAddr ||
    (lastConnectTime > 1.0 && now() - lastConnectTime > FLOW_KNOBS->ALWAYS_ACCEPT_DELAY)) {
    connect.cancel(); prependConnectPacket();
    connect = connectionKeeper(self, conn, reader);   // keep the new connection
} else {
    reader.cancel(); conn->close();                     // keep our prior connection
}
```

---

## 5. Packet Queue Types

Defined in `flow/include/flow/Net2Packet.h`.

### UnsentPacketQueue

Linked list of `PacketBuffer` nodes. Fire-and-forget: discarded on connection failure.

```cpp
class UnsentPacketQueue : NonCopyable {
    PacketBuffer *unsent_first, *unsent_last;

    PacketBuffer* getWriteBuffer(size_t sizeHint = 0);  // Append point
    void setWriteBuffer(PacketBuffer* pb);               // Update tail after writes
    PacketBuffer* getUnsent() const;                     // Head for sending
    void sent(int bytes);                                // Advance after send
    void discardAll();                                   // Drop everything
    bool empty() const;
};
```

`connectionWriter` drains from `getUnsent()`, calling `sent(bytes)` as data goes out.

### ReliablePacketList

Circular doubly-linked list of `ReliablePacket` nodes. Survives connection failures and
gets re-sent after reconnection.

```cpp
class ReliablePacketList : NonCopyable {
    ReliablePacket reliable;  // Sentinel node

    bool empty() const { return reliable.next == &reliable; }
    void insert(ReliablePacket* rp);
    PacketBuffer* compact(PacketBuffer* into, PacketBuffer* stopAt);
};
```

`compact()` copies already-sent reliable packets back into the unsent queue for
retransmission after reconnect.

### PacketBuffer

Arena-style buffer used by both queues. Tracks `bytes_written` and `bytes_sent` cursors.
Chained via `next` pointer. Created with `PacketBuffer::create(sizeHint)`.

---

## 6. Wire Protocol

### Packet framing

Every message on the wire:

```
+-----------------------------------+
| uint32_t packetLength             |  Payload bytes only (excludes this header)
+-----------------------------------+
| uint64_t checksum                 |  XXH3-64 of the payload; omitted on TLS
+-----------------------------------+
| UID token (16 bytes)              |  Destination endpoint (first payload bytes)
+-----------------------------------+
| Serialized message payload        |  FlatBuffers encoding
+-----------------------------------+
```

The checksum is `XXH3_64bits` (not CRC32C), computed in `sendPacket` and verified in
`scanPackets` (`FlowTransport.cpp:1260`, check at `:1346`). It is only present when the
connection is not TLS (`checksumEnabled = !peerAddress.isTLS()`), since TLS already
authenticates the stream.

### ConnectPacket handshake

First packet on every new connection. Not framed like regular packets.

```cpp
#pragma pack(push, 1)
struct ConnectPacket {
    uint32_t connectPacketLength;
    ProtocolVersion protocolVersion;   // 64-bit, e.g. 0x0FDB00A400040001
    uint16_t canonicalRemotePort;
    uint64_t connectionId;            // Pairs a multi-version client's connections; else 0
    uint32_t canonicalRemoteIp4;      // + flags / canonicalRemoteIp6 (FlowTransport.cpp:550)
};
#pragma pack(pop)
```

The receiver validates `protocolVersion` for compatibility before proceeding. Incompatible
versions cause immediate connection close.

---

## 7. Connection Actors

Four actors manage each connection in a hierarchy:

```
Peer
 +-- connectionKeeper    (lifecycle coordinator)
      +-- connectionWriter   (drain unsent queue -> socket)
      +-- connectionReader   (socket -> scanPackets -> endpoint dispatch)
      +-- connectionMonitor  (ping/pong health checks)
```

### connectionKeeper -- lifecycle coordinator

```cpp
ACTOR Future<Void> connectionKeeper(Reference<Peer> self, TransportData* transport) {
    state double reconnectionDelay = FLOW_KNOBS->INITIAL_RECONNECTION_TIME;

    loop {
        try {
            Reference<IConnection> conn = wait(
                INetworkConnections::net()->connect(self->destination));

            self->connection = conn;
            reconnectionDelay = FLOW_KNOBS->INITIAL_RECONNECTION_TIME;  // Reset on success

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

The `||` operator means "run all three concurrently, return when any completes (with error)."
Any actor failure tears down all three, and connectionKeeper retries.

### connectionReader -- receive path

Reads from the socket, calls `scanPackets()` which:
1. Validates the XXH3-64 checksum (non-TLS connections only).
2. Extracts the 16-byte UID token.
3. Looks up the receiver in `EndpointMap`.
4. Calls `receiver->receive(data)`.

### connectionWriter -- send path

Drains `peer->unsent` queue into the socket. Wakes when new packets are enqueued.

### connectionMonitor -- health checks

```cpp
ACTOR Future<Void> connectionMonitor(Reference<Peer> peer) {
    loop {
        wait(delay(FLOW_KNOBS->CONNECTION_MONITOR_LOOP_TIME));
        if (!peer->connection) break;

        state double pingStart = now();
        try {
            wait(peer->connection->ping());
            peer->pingLatencies.addSample(now() - pingStart);
        } catch (Error& e) {
            peer->timeoutCount++;
            if (peer->timeoutCount > threshold ||
                now() - peer->lastDataPacketSentTime > CONNECTION_MONITOR_TIMEOUT)
                throw connection_failed();
        }

        // Close idle unreferenced connections
        if (now() - peer->lastDataPacketSentTime > CONNECTION_MONITOR_IDLE_TIMEOUT &&
            peer->peerReferences == 0)
            break;
    }
}
```

---

## 8. Reconnection with Exponential Backoff

Algorithm inside `connectionKeeper`:

1. On connection failure, wait `reconnectionDelay`.
2. Multiply delay by `RECONNECTION_TIME_GROWTH_RATE`.
3. Cap at `MAX_RECONNECTION_TIME`.
4. On successful connection, reset delay to `INITIAL_RECONNECTION_TIME`.
5. If connected for longer than `RECONNECTION_RESET_TIME`, reset delay.

Sequence with default knobs: 50ms, 60ms, 72ms, 86ms, ... capped at 500ms.

---

## 9. sendReliable vs sendUnreliable

| Aspect               | `sendUnreliable`                          | `sendReliable`                           |
|-----------------------|-------------------------------------------|------------------------------------------|
| Queue                 | `peer->unsent` (UnsentPacketQueue)        | `peer->reliable` (ReliablePacketList)    |
| On disconnect         | Discarded                                 | Re-sent after reconnect via `compact()`  |
| Use case              | `send`/`tryGetReply` requests, all replies | `getReply` requests (at-least-once)      |
| Ordering              | Preserved within queue                    | Preserved within queue                   |

`sendUnreliable` is the common path. The caller is expected to handle retries (e.g., via
`retryBrokenPromise`). `sendReliable` backs `RequestStream::getReply`: the *request* is
retained and re-sent after reconnect until `sendCanceler` cancels it. Replies are **not**
reliable: `networkSender` (`networksender.h`) sends every `ReplyPromise` value or error
with `sendUnreliable`.

---

## 10. Local vs Remote Routing

```cpp
// FlowTransport.cpp:1949
static void sendLocal(TransportData* self, ISerializeSource const& what, const Endpoint& destination) {
    ObjectWriter wr(AssumeVersion(g_network->protocolVersion()));
    what.serializeObjectWriter(wr);        // local messages ARE serialized
    // ... copied into an arena and handed to deliver() as a scheduled task
}
```

`sendReliable`/`sendUnreliable` call `sendLocal` when the destination is a local address.
Local transport delivery skips wire framing, the checksum and the connection actors, but
**not** serialization: the message is written with `ObjectWriter` and dispatched through the
same `deliver` path as a remote packet. The one true shortcut is above the transport: a
`RequestStream` whose queue is local (`!queue->isRemoteEndpoint()`) pushes the typed value
straight into its queue.

---

## 11. Message Flow Trace

### Real network path (Net2)

```
1. App calls sendUnreliable(message, endpoint)
2. FlowTransport::sendUnreliable()
   - Lookup/create Peer for destination address
   - Serialize message with BinaryWriter
   - Add to peer->unsent queue
3. connectionWriter actor
   - Pop from unsent queue
   - Call connection->write() with SendBuffer
4. Net2::Connection::write()
   - Write to boost::asio socket
   - Returns bytes written
5. TCP stack transmits over real network
6. Remote Net2::Connection::read()
   - Boost.ASIO callback on data arrival
7. connectionReader actor
   - Read from connection->read()
   - Call scanPackets() to parse
8. scanPackets()
   - Validate XXH3-64 checksum (non-TLS only)
   - Extract endpoint token (UID)
   - Lookup receiver in EndpointMap
   - Call receiver->receive(data)
```

### Simulated path (Sim2)

```
1. App calls sendUnreliable(message, endpoint)          -- identical
2. FlowTransport::sendUnreliable()                      -- identical
3. connectionWriter actor                                -- identical
4. Sim2Conn::write()
   - Add bytes to unsent queue
   - Update sentBytes AsyncVar (triggers receiver)
   - Returns bytes "written" (immediate, no real I/O)
5. Sim2 receiver actor (on peer process)
   - Wait for sentBytes.onChange()
   - Wait random latency via delay()
   - Update receivedBytes (triggers onReadable)
6. connectionReader actor                                -- identical
7. scanPackets()                                         -- identical
8. Endpoint dispatch                                     -- identical
```

Steps 4-5 are the only difference: real TCP is replaced with deterministic,
latency-injected in-memory transfer. FlowTransport code is completely unaware of which
runtime it runs on.

---

## 12. Configuration Knobs

| Knob                                | Default (real) | Default (sim)  | Purpose                                  |
|--------------------------------------|----------------|----------------|------------------------------------------|
| `INITIAL_RECONNECTION_TIME`          | 0.05s          | 0.05s          | First backoff delay                      |
| `MAX_RECONNECTION_TIME`              | 0.5s           | 0.5s           | Backoff cap                              |
| `RECONNECTION_TIME_GROWTH_RATE`      | 1.2            | 1.2            | Exponential multiplier                   |
| `RECONNECTION_RESET_TIME`            | 5.0s           | 5.0s           | Reset backoff after this much uptime     |
| `CONNECTION_MONITOR_LOOP_TIME`       | 1.0s           | 0.75s          | Ping interval                            |
| `CONNECTION_MONITOR_TIMEOUT`         | 2.0s           | 1.5s           | Failure detection threshold              |
| `CONNECTION_MONITOR_IDLE_TIMEOUT`    | 180.0s         | 180.0s         | Close unused connections                 |
| `CONNECTION_ID_TIMEOUT`              | 600.0s         | 600.0s         | Forget old connection IDs                |
| `ACCEPT_BATCH_SIZE`                  | 20             | 20             | Connections accepted per listener wakeup  |
