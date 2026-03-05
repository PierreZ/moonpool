# Layer 3: fdbrpc -- RPC Semantics, Delivery Modes, and Application Patterns

## Layer Position

```
+-----------------------------------------------------------+
| Layer 4: Application                                      |
|   ClusterController, Worker, NativeAPI                    |
|   Chooses which Layer 2/3 API to call                     |
|   Adds generation numbers, idempotency IDs                |
+-----------------------------------------------------------+
| Layer 3: Load Balancer + MultiInterface          <--------+
|   LoadBalance.actor.h, MultiInterface.h, QueueModel.h    |
|   Locality-aware routing, hedged requests, AtMostOnce     |
+-----------------------------------------------------------+
| Layer 2: fdbrpc (RequestStream + ReplyPromise)   <--------+-- THIS FILE
|   fdbrpc.h, genericactors.actor.h, FailureMonitor.h      |
|   4 delivery modes, failure tracking, retry helpers       |
+-----------------------------------------------------------+
| Layer 1: FlowTransport                                    |
|   sendReliable / sendUnreliable, Peer, connectionKeeper   |
|   TCP management, ping, disconnect signaling              |
+-----------------------------------------------------------+
```

fdbrpc is a thin layer between FlowTransport (raw bytes over TCP) and application code.
It adds **request/response pairing** and **delivery semantics** on top of a dumb transport.

FlowTransport gives you: `sendReliable(bytes, endpoint)`, `sendUnreliable(bytes, endpoint)`,
`peer.disconnect` signal, and `IFailureMonitor::notifyDisconnect(addr)`. No message IDs,
no request/response correlation, no retry logic.

---

## 1. Core Types

### FlowReceiver / NetworkMessageReceiver (base)

```cpp
// fdbrpc/include/fdbrpc/fdbrpc.h
struct NetworkMessageReceiver {
    virtual void receive(ArenaObjectReader& reader) = 0;
    virtual bool isStream() const = 0;
};

struct FlowReceiver : NetworkMessageReceiver {
    Endpoint endpoint;
    bool isLocalEndpoint;

    void makeWellKnownEndpoint(UID token, TaskPriority priority) {
        endpoint.token = token;
        endpoint.addresses = FlowTransport::transport().getLocalAddresses();
        FlowTransport::transport().addWellKnownEndpoint(endpoint, this, priority);
    }
};
```

### NetSAV\<T\> (single-value reply)

The network-aware single assignment variable. Backs `ReplyPromise<T>` -- essentially a
oneshot channel whose receive side is registered in the endpoint map. When a reply packet
arrives, FlowTransport looks up the token, finds the NetSAV, and delivers the value.

### NetNotifiedQueue\<T\> (stream messages)

Type `T` is baked into the queue at compile time. When a packet arrives, the deserializer
does not need to figure out what type to read -- the queue already knows.

```cpp
// fdbrpc/include/fdbrpc/fdbrpc.h
template<class T>
struct NetNotifiedQueue final : NotifiedQueue<T>, FlowReceiver {
    void receive(ArenaObjectReader& reader) override {
        T message;
        reader.deserialize(message);  // TYPE T IS KNOWN HERE
        this->send(std::move(message));
    }
    bool isStream() const override { return true; }
};
```

### ReplyPromise\<T\> (oneshot + endpoint token)

A one-shot channel where the server sends back a response, routable over the network
via an endpoint token.

```
ReplyPromise<T> = oneshot::Sender<Result<T, Error>> + Endpoint
```

- **Created by the client** as part of a request struct
- **Serialized** as just the endpoint token (UID) when sent over the network
- **On the server**: calling `reply.send(value)` serializes value and sends it to the endpoint
- **On the client**: the matching `Future<T>` resolves when the reply packet arrives
- **On drop without send**: delivers `broken_promise` error to the waiter

### RequestStream\<T\> (handle to remote inbox)

```cpp
// fdbrpc/include/fdbrpc/fdbrpc.h:726-835
template<class T>
class RequestStream {
    NetNotifiedQueue<T>* queue;
public:
    Endpoint getEndpoint() const { return queue->endpoint; }
    FutureStream<T> getFuture() const { return FutureStream<T>(queue); }
    void send(T&& message) { queue->send(std::move(message)); }
};
```

Wraps an `Endpoint` and provides 4 delivery modes (see section 5).

---

## 2. The networkSender Pattern

The most elegant part of fdbrpc. `ReplyPromise<T>` has **different serialization behavior
for sending vs receiving**, controlled by `serializable_traits`:

```cpp
// fdbrpc/include/fdbrpc/fdbrpc.h
template<class T>
struct serializable_traits<ReplyPromise<T>> : std::true_type {
    template<class Archiver>
    static void serialize(Archiver& ar, ReplyPromise<T>& p) {
        if constexpr (Archiver::isDeserializing) {
            // SERVER SIDE: reconstruct endpoint pointing back to client
            UID token;
            serializer(ar, token);
            auto endpoint = FlowTransport::transport().loadedEndpoint(token);
            p = ReplyPromise<T>(endpoint);
            // THE MAGIC: spawn actor that sends reply when promise is fulfilled
            networkSender(p.getFuture(), endpoint);
        } else {
            // CLIENT SIDE: just serialize the token (16 bytes)
            const auto& ep = p.getEndpoint().token;
            serializer(ar, ep);
        }
    }
};
```

The `networkSender` actor waits for the local promise to be fulfilled, then sends the
result over the network:

```cpp
ACTOR template<class T>
void networkSender(Future<T> input, Endpoint endpoint) {
    try {
        T value = wait(input);
        FlowTransport::transport().sendUnreliable(
            SerializeSource<ErrorOr<T>>(value), endpoint, false);
    } catch (Error& err) {
        // Including broken_promise -- forward error to remote caller
        FlowTransport::transport().sendUnreliable(
            SerializeSource<ErrorOr<T>>(err), endpoint, false);
    }
}
```

---

## 3. Complete RPC Message Flow

```
CLIENT                                          SERVER
------                                          ------
 1. Create request with ReplyPromise
    { key: "mykey", reply: ReplyPromise }
    reply has endpoint pointing to client

 2. Serialize request
    key serialized normally
    ReplyPromise serializes ONLY its token (16 bytes)

 3. Send to server endpoint
    ------------------------------------------------->

                                                 4. Receive packet, extract UID
                                                 5. Lookup receiver by token in EndpointMap
                                                 6. Deserialize into typed request
                                                    ReplyPromise deserialization spawns
                                                    networkSender actor

                                                 7. Handler processes request:
                                                    req.reply.send(value);

                                                 8. networkSender actor wakes up,
                                                    sends result to client endpoint
    <-------------------------------------------------

 9. Client FlowTransport receives packet
    scanPackets extracts token
    EndpointMap[token] -> NetSAV
    NetSAV.send(value)

10. Original future resolves with value
```

---

## 4. The 4 Delivery Modes

All live on `RequestStream<T>` in `fdbrpc/include/fdbrpc/fdbrpc.h`.

| Method | Guarantee | Transport | On Connection Drop |
|--------|-----------|-----------|-------------------|
| `send()` | Fire-and-forget | sendUnreliable | Silently lost |
| `tryGetReply()` | At-most-once | sendUnreliable | `request_maybe_delivered` |
| `getReply()` | At-least-once | sendReliable | Retransmits; server may see duplicates |
| `getReplyUnlessFailedFor()` | At-least-once + timeout | sendReliable | `request_maybe_delivered` after duration |

### 4a. send() -- fire-and-forget (line 733)

```cpp
void send(U&& value) const {
    if (queue->isRemoteEndpoint()) {
        FlowTransport::transport().sendUnreliable(
            SerializeSource<T>(std::forward<U>(value)), getEndpoint(), true);
    } else
        queue->send(std::forward<U>(value));
}
```

No reply expected. Zero or one delivery. Used for: heartbeats, notifications, triggers.

### 4b. tryGetReply() -- at-most-once (line 785)

Uses `waitValueOrSignal()` (`genericactors.actor.h:362-398`):

```cpp
Future<ErrorOr<REPLY_TYPE(X)>> tryGetReply(const X& value) const {
    if (queue->isRemoteEndpoint()) {
        Future<Void> disc = makeDependent<T>(IFailureMonitor::failureMonitor())
            .onDisconnectOrFailure(getEndpoint());
        if (disc.isReady()) {
            return ErrorOr<REPLY_TYPE(X)>(request_maybe_delivered());
        }
        Reference<Peer> peer = FlowTransport::transport().sendUnreliable(
            SerializeSource<T>(value), getEndpoint(), true);
        auto& p = getReplyPromise(value);
        return waitValueOrSignal(p.getFuture(), disc, getEndpoint(), p, peer);
    }
    // ...local path...
}
```

Pseudocode for `waitValueOrSignal`:

```
select! {
    reply = reply_future   => Ok(reply)
    _     = disconnect     => Err(request_maybe_delivered)
    // broken_promise      => mark endpoint not found, Err(request_maybe_delivered)
}
```

### 4c. getReply() -- at-least-once (line 752)

Uses `sendCanceler()` (`genericactors.actor.h:400-431`):

```cpp
Future<REPLY_TYPE(X)> getReply(const X& value) const {
    if (queue->isRemoteEndpoint()) {
        return sendCanceler(getReplyPromise(value),
            FlowTransport::transport().sendReliable(SerializeSource<T>(value), getEndpoint()),
            getEndpoint());
    }
    send(value);
    return reportEndpointFailure(getReplyPromise(value).getFuture(), getEndpoint());
}
```

Pseudocode for `sendCanceler`:

```
loop {
    if permanently_failed(endpoint) {
        cancel_reliable(packet);
        pending!().await;  // will become broken_promise when dropped
    }
    select! {
        reply = reply_future            => { cancel_reliable(packet); return reply }
        _     = failure_state_changed() => continue  // re-check
    }
}
```

The reliable packet sits in `peer.reliable` queue. If the connection drops and reconnects,
FlowTransport automatically retransmits it. The server may receive the request multiple times.

### 4d. getReplyUnlessFailedFor() -- at-least-once with timeout (line 860)

Combines `getReply()` with failure monitor timeout:

```
select! {
    reply = getReply(request)                                  => Ok(reply)
    _     = failure_monitor.onFailedFor(endpoint, dur, slope)  => Err(request_maybe_delivered)
}
```

Used for singleton RPCs (recruitment, registration) where you want reliable delivery
but cannot wait forever if the endpoint dies.

---

## 5. FailureMonitor

**Files**: `fdbrpc/include/fdbrpc/FailureMonitor.h`, `FailureMonitor.actor.cpp`

Two-tier tracking -- reactive, not probing:

```
FailureMonitor {
    // Address-level: is this machine/process reachable?
    address_status: HashMap<NetworkAddress, FailureStatus>,

    // Endpoint-level: does this specific endpoint exist?
    failed_endpoints: HashMap<Endpoint, FailureReason>,  // NOT_FOUND or UNAUTHORIZED
}
```

Key methods and which delivery mode consumes them:

| Method | Used By |
|--------|---------|
| `notifyDisconnect(addr)` | Called by FlowTransport on TCP drop |
| `endpointNotFound(endpoint)` | Called when broken_promise received |
| `onDisconnectOrFailure(endpoint) -> Future<()>` | `tryGetReply()` |
| `permanentlyFailed(endpoint) -> bool` | `sendCanceler()` / `getReply()` |
| `onFailedFor(endpoint, duration, slope) -> Future<()>` | `getReplyUnlessFailedFor()` |
| `onStateChanged(endpoint) -> Future<()>` | `sendCanceler()` to re-evaluate |

The failure monitor does NOT actively probe. It reacts to signals from FlowTransport
(connection drops) and from RPC responses (endpoint not found).

---

## 6. Error Handling

### broken_promise

Occurs when all `Promise` objects are destroyed without fulfillment. The `networkSender`
actor catches this and forwards the error to the remote caller via `sendUnreliable`.

### request_maybe_delivered (error 1030)

```cpp
// flow/include/flow/error_definitions.h:57
ERROR( request_maybe_delivered, 1030, "Request may or may not have been delivered" )
```

FDB makes the maybe-delivered ambiguity **explicit** rather than hiding it behind a
generic timeout. This is the central design principle of the entire RPC stack.

### WaitFailure Pattern

Actors hold `ReplyPromise<Void>` without responding. If the actor dies, callers get
`broken_promise` -- a cheap liveness detection mechanism:

```cpp
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

## 7. Interface Pattern

Interfaces bundle multiple `RequestStream<T>` endpoints into a single serializable struct.
Only one base endpoint is serialized; others derive via **endpoint adjustment**:

```cpp
// Example: StorageServerInterface
struct StorageServerInterface {
    RequestStream<GetValueRequest> getValue;
    RequestStream<GetKeyRequest> getKey;
    RequestStream<GetKeyValuesRequest> getKeyValues;

    template<class Ar>
    void serialize(Ar& ar) {
        serializer(ar, uniqueID, locality, getValue);
        if constexpr (Ar::isDeserializing) {
            getKey = RequestStream<GetKeyRequest>(
                getValue.getEndpoint().getAdjustedEndpoint(1));
            getKeyValues = RequestStream<GetKeyValuesRequest>(
                getValue.getEndpoint().getAdjustedEndpoint(2));
        }
    }
};
```

`getAdjustedEndpoint(offset)` adds offset to the first part of the UID:

```
Base token:              (0x123456789ABCDEF0, 0xFEDCBA9876543210)
getAdjustedEndpoint(1):  (0x123456789ABCDEF1, 0xFEDCBA9876543210)
getAdjustedEndpoint(2):  (0x123456789ABCDEF2, 0xFEDCBA9876543210)
```

Saves 14 bytes per additional RequestStream in the interface.

---

## 8. Bootstrap Mechanism

### Well-known endpoint tokens

```cpp
constexpr UID WLTOKEN_CLIENTLEADERREG_GETLEADER(-1, 2);
constexpr UID WLTOKEN_CLIENTLEADERREG_OPENDATABASE(-1, 3);
constexpr int WLTOKEN_RESERVED_COUNT = 64;
```

Hardcoded constants -- any client with the same version can compute them.

### Bootstrap sequence

```
1. READ CLUSTER FILE
   "description:ID@coordinator1:4500,coordinator2:4500,coordinator3:4500"

2. CONSTRUCT COORDINATOR ENDPOINTS
   For each coordinator: Endpoint { addr, WLTOKEN_CLIENTLEADERREG_GETLEADER }

3. CONTACT COORDINATORS
   Send GetLeaderRequest to all; wait for majority agreement

4. GET CLUSTER CONTROLLER INTERFACE
   LeaderInfo response contains ClusterControllerInterface

5. OPEN DATABASE
   Contact ClusterController; receive CommitProxyInterface, GrvProxyInterface, etc.

6. READY FOR OPERATIONS
```

### ServerDBInfo broadcast

Rather than per-service discovery, the ClusterController broadcasts a `ServerDBInfo` struct
containing all critical interfaces to every worker:

```cpp
struct ServerDBInfo {
    ClusterControllerInterface clusterInterface;
    MasterInterface master;
    vector<CommitProxyInterface> commitProxies;
    vector<GrvProxyInterface> grvProxies;
    vector<ResolverInterface> resolvers;
};
```

Workers subscribe to updates and always have current interface information.

---

## 9. Retry Helpers

### retryBrokenPromise -- for well-known endpoints

`genericactors.actor.h:39-57`

```cpp
ACTOR template <class Req, bool P>
Future<REPLY_TYPE(Req)> retryBrokenPromise(RequestStream<Req, P> to, Req request) {
    loop {
        try {
            REPLY_TYPE(Req) reply = wait(to.getReply(request));
            return reply;
        } catch (Error& e) {
            if (e.code() != error_code_broken_promise)
                throw;
            resetReply(request);
            wait(delayJittered(FLOW_KNOBS->PREVENT_FAST_SPIN_DELAY));
        }
    }
}
```

Only for well-known endpoints (coordinators, cluster controller) that restart at the
same token. NOT for ephemeral endpoints.

### brokenPromiseToNever -- suppress broken_promise

Used in worker registration (`worker.actor.cpp:695`): wraps a future so that
`broken_promise` becomes `Never` (pending forever), causing the caller to fall through
to a timeout or interface-change branch instead.

---

## 10. Load Balancing

### MultiInterface\<T\>

A group of alternative servers for the same logical service:

```
MultiInterface<ReferencedInterface<StorageServerInterface>> {
    alternatives: Vec<(interface, distance)>  // sorted by locality distance
    countBest: usize  // number of alternatives in the same DC
}
```

Sorted by distance: `SAME_DC < SAME_MACHINE < REMOTE`.

### QueueModel

Per-endpoint latency and queue depth tracking:

- `smoothOutstanding` -- smoothed count of outstanding requests
- `latency` -- smoothed request latency
- `penalty` -- server-reported penalty ("I'm overloaded, avoid me")
- `failedUntil` -- time until which this endpoint should be avoided

Fed by `ModelHolder` which tracks request start/end times.

### loadBalance() algorithm

`LoadBalance.actor.h:690`:

```
loadBalance(
    alternatives: MultiInterface,
    channel: member pointer,      // which RequestStream on the interface
    request: Request,
    atMostOnce: bool,
    model: QueueModel,
)
```

1. **Pick bestAlt**: lowest `smoothOutstanding` from QueueModel (or random). Prefer local DC.
2. **Pick nextAlt**: second-best server for hedging.
3. **Send to bestAlt** via `tryGetReply()`.
4. **Hedge**: if bestAlt is slow, race 2nd request to nextAlt after delay computed from
   latency model. If bestAlt latency >> nextAlt, send immediately.
5. **On failure**:
   - `request_maybe_delivered` or `broken_promise`:
     - `atMostOnce == true` -> propagate to caller (might have been processed)
     - `atMostOnce == false` -> retry on next alternative
   - `server_overloaded` -> retry
   - Other errors -> propagate to caller
6. **Cycle** through alternatives until success or all tried.
7. **Backoff** with `allAlternativesFailedDelay` if everything failed.

### AtMostOnce flag -- the key design decision

```cpp
// LoadBalance.actor.h:591-618
bool maybeDelivered = errCode == error_code_broken_promise ||
                      errCode == error_code_request_maybe_delivered;
if (atMostOnce && maybeDelivered) {
    return request_maybe_delivered();  // Don't retry
}
```

- **`AtMostOnce::True`** (commits): "This request has side effects. If maybe-delivered,
  do NOT retry on another server -- tell me so I can handle it."
- **`AtMostOnce::False`** (reads): "This request is idempotent. If maybe-delivered,
  just try another server."

### Application API selection table

| Use case | API | Example |
|----------|-----|---------|
| Notification, no reply needed | `stream.send()` | heartbeat, trigger |
| Single known server, accept failure | `stream.tryGetReply()` | one-off probe |
| Single known server, must deliver | `stream.getReply()` | TLog rejoin |
| Single server, with failure timeout | `stream.getReplyUnlessFailedFor()` | singleton recruitment |
| Well-known endpoint, retry forever | `retryBrokenPromise(stream, req)` | coordinator RPC |
| Multiple equivalent servers, idempotent | `loadBalance(..., AtMostOnce::False)` | reads |
| Multiple equivalent servers, side effects | `loadBalance(..., AtMostOnce::True)` | commits |

---

## 11. The 6 RPC Strategies

### Strategy 1: Idempotent-by-Design

Design the request to describe the **desired end state**, not a delta. Re-delivery is harmless.

- Worker registration: "I am worker X with these capabilities"
- TLog rejoin (`TLogServer.actor.cpp:~2599`): "TLog X is alive and wants to rejoin"
- Master registration: "Master X with this config exists"

Use whenever you can reformulate the operation as a state assertion.

### Strategy 2: Generation / Sequence Number Dedup

Tag each request with a monotonic number. Server tracks last-seen number per client.

```cpp
// worker.actor.cpp:615-639
RegisterWorkerRequest request(
    interf, initialClass, processClass, asyncPriorityInfo->get(),
    requestGeneration++,  // monotonically increasing
    ddInterf->get(), rkInterf->get(), /* ... */);
```

CC checks `req.generation >= info->second.gen` (`ClusterController.actor.cpp:1292`).
Old generations are ignored.

### Strategy 3: Fire-and-Forget

Use `.send()` for RPCs where losing the message is tolerable. Heartbeats, notifications,
advisory data. Missing one is fine; the next one compensates.

### Strategy 4: Read-Before-Retry (Commit Idempotency)

On `request_maybe_delivered`, read the server's state to determine if the previous
request succeeded before retrying.

```cpp
// NativeAPI.actor.cpp:6829-6866
if (e.code() == error_code_request_maybe_delivered ||
    e.code() == error_code_commit_unknown_result) {
    if (req.idempotencyId.valid()) {
        Optional<CommitResult> commitResult = wait(determineCommitStatus(
            trState, req.transaction.read_snapshot, /* ... */,
            req.idempotencyId));
        if (commitResult.present()) {
            return Void();  // Commit succeeded -- return cached version
        } else {
            throw transaction_too_old();  // Commit did not happen
        }
    }
    throw commit_unknown_result();
}
```

### Strategy 5: Well-Known Endpoint + retryBrokenPromise

For endpoints that survive process restarts, catch `broken_promise` and retry
indefinitely with jitter. Used for coordinators, cluster controller, config transactions.

### Strategy 6: Load-Balanced with AtMostOnce Flag

Use the load balancer to try alternative servers. `AtMostOnce::True` for non-idempotent
ops (commits), `False` for idempotent ones (reads).

---

## 12. Decision Flowchart

```
Is losing the message acceptable?
  YES -> Strategy 3: .send(), fire-and-forget
  NO  |
      v
Can you reformulate as "set state = X"?
  YES -> Strategy 1: idempotent-by-design + .getReply()
  NO  |
      v
Can the server track per-client sequence numbers?
  YES -> Strategy 2: generation dedup + .getReply()
  NO  |
      v
Can you read the state after failure to check?
  YES -> Strategy 4: .tryGetReply() + read-before-retry
  NO  |
      v
Are there multiple equivalent servers?
  YES -> Strategy 6: loadBalance with AtMostOnce::True + handle error at caller
  NO  -> Strategy 5 (if well-known) or Strategy 2 (add server state)
```

---

## 13. Rust Mapping Cheat Sheet

| C++ (fdbrpc) | Rust equivalent |
|---|---|
| `ReplyPromise<T>` | `oneshot::Sender<Result<T,E>>` + Endpoint |
| `Future<T>` (FDB) | `oneshot::Receiver<Result<T,E>>` |
| `RequestStream<T>` | newtype around `Endpoint` + transport ref |
| `SAV<T>` / `NetSAV<T>` | The oneshot channel internals |
| `NetNotifiedQueue<T>` | `mpsc` channel + `Waker` notification |
| `waitValueOrSignal` | `tokio::select!` on reply vs disconnect |
| `sendCanceler` | `tokio::select!` on reply vs failure loop |
| `IFailureMonitor` | Trait with `watch`-based notifications |
| `EndpointMap` | `HashMap<Token, Box<dyn MessageReceiver>>` |
| `broken_promise` on drop | `impl Drop` that sends Err to receiver |
| `ReliablePacket` | Transport's reliable send handle |
| `Peer.disconnect` | `tokio::sync::watch` or `broadcast` |
| `loadBalance()` | Async fn with `select!` + retry loop |
| `QueueModel` | Struct with smoothed latency/outstanding |
| `MultiInterface<T>` | `Vec<(Interface, Distance)>` sorted by locality |
| `AtMostOnce` | Bool/enum parameter on load balance fn |

---

## Key Source Files

| Component | Path |
|-----------|------|
| 4 delivery modes | `fdbrpc/include/fdbrpc/fdbrpc.h:726-826` |
| waitValueOrSignal | `fdbrpc/include/fdbrpc/genericactors.actor.h:362-398` |
| sendCanceler | `fdbrpc/include/fdbrpc/genericactors.actor.h:400-431` |
| retryBrokenPromise | `fdbrpc/include/fdbrpc/genericactors.actor.h:39-57` |
| FailureMonitor | `fdbrpc/include/fdbrpc/FailureMonitor.h` |
| Load balancer | `fdbrpc/include/fdbrpc/LoadBalance.actor.h:583-625` |
| Error definitions | `flow/include/flow/error_definitions.h` |
| Worker registration (Strategy 2) | `fdbserver/worker.actor.cpp:615-651` |
| CC generation dedup (Strategy 2) | `fdbserver/ClusterController.actor.cpp:1290-1352` |
| Commit idempotency (Strategy 4) | `fdbclient/NativeAPI.actor.cpp:6829-6871` |
