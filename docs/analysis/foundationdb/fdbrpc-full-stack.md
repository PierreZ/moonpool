# The Full fdbrpc Stack: From Bytes to Application RPCs

## The 4 Layers

```
┌─────────────────────────────────────────────────────────┐
│ Layer 4: Application                                    │
│   ClusterController, Worker, NativeAPI                  │
│   - Chooses which layer 2/3 API to call                 │
│   - Adds generation numbers, idempotency IDs            │
│   - Defines Interface structs bundling RequestStreams    │
├─────────────────────────────────────────────────────────┤
│ Layer 3: Load Balancer + MultiInterface                 │
│   LoadBalance.actor.h, MultiInterface.h, QueueModel.h  │
│   - Groups of equivalent servers (e.g. storage replicas)│
│   - Locality-aware routing (prefer local DC)            │
│   - Latency-based server selection via QueueModel       │
│   - Hedged requests (race 2nd server if 1st is slow)    │
│   - AtMostOnce flag controls retry-on-maybe-delivered   │
│   - TSS comparison (testing storage server validation)  │
├─────────────────────────────────────────────────────────┤
│ Layer 2: fdbrpc (RequestStream + ReplyPromise)          │
│   fdbrpc.h, genericactors.actor.h                       │
│   - 4 delivery modes on RequestStream                   │
│   - ReplyPromise: oneshot reply routed by endpoint token│
│   - FailureMonitor: endpoint/address failure tracking   │
│   - Retry helpers: retryBrokenPromise, hostname retry   │
├─────────────────────────────────────────────────────────┤
│ Layer 1: FlowTransport                                  │
│   FlowTransport.h, FlowTransport.actor.cpp              │
│   - TCP connection management + reconnect               │
│   - sendReliable / sendUnreliable packet queues         │
│   - Ping-based liveness, disconnect signaling           │
│   - Packet framing, checksum, endpoint dispatch         │
└─────────────────────────────────────────────────────────┘
```

---

## Layer 1: FlowTransport (you already have this)

Dumb pipe. Two send modes:

- `sendReliable(bytes, endpoint)` → queued in `peer.reliable`, retransmitted on reconnect
- `sendUnreliable(bytes, endpoint)` → queued in `peer.unsent`, dropped on disconnect

Signals: `peer.disconnect` promise, `IFailureMonitor::notifyDisconnect(addr)`.

No message IDs, no request/response correlation, no retry logic.

---

## Layer 2: fdbrpc — Request/Response Pairing

**Files**: `fdbrpc.h`, `genericactors.actor.h`, `FailureMonitor.h`

### Core types

**ReplyPromise\<T\>** — a network-routable oneshot. The client allocates an endpoint token,
the server sends the reply to that token. Backed by `NetSAV<T>` (flow.h).

**RequestStream\<T\>** — a handle to a remote endpoint. Wraps an Endpoint and provides
4 delivery modes.

### The 4 delivery modes

All live on `RequestStream<T>` in fdbrpc.h:

#### 1. `send()` (line 730)
Fire-and-forget. Uses `sendUnreliable`. No reply. 0 or 1 delivery.

#### 2. `tryGetReply()` (line 779)
At-most-once. Uses `sendUnreliable`. Returns `ErrorOr<Reply>`.

Implementation is `waitValueOrSignal()` (genericactors.actor.h:362):
```
select! {
    reply = reply_future   => Ok(reply)
    _     = disconnect     => Err(request_maybe_delivered)
    // broken_promise      => mark endpoint not found, Err(request_maybe_delivered)
}
```

#### 3. `getReply()` (line 751)
At-least-once. Uses `sendReliable`. Returns `Reply` or throws.

Implementation is `sendCanceler()` (genericactors.actor.h:400):
```
loop {
    if permanently_failed(endpoint) {
        cancel_reliable(packet);
        pending!().await;  // will become broken_promise when dropped
    }
    select! {
        reply = reply_future              => { cancel_reliable(packet); return reply }
        _     = failure_state_changed()   => continue  // re-check
    }
}
```

The reliable packet retransmits automatically on reconnect. Server may receive duplicates.

#### 4. `getReplyUnlessFailedFor(duration, slope)` (line 860)
At-least-once with timeout. Combines `getReply()` with failure monitor timeout:
```
select! {
    reply = getReply(request)                              => Ok(reply)
    _     = failure_monitor.onFailedFor(endpoint, dur, slope) => Err(request_maybe_delivered)
}
```

Used for singleton RPCs (recruitment, registration) where you want reliable delivery
but can't wait forever if the endpoint dies.

### FailureMonitor

Two-tier tracking:
- **Address-level**: is the machine reachable? (set by `notifyDisconnect`)
- **Endpoint-level**: does this specific endpoint exist? (set by `endpointNotFound`)

Key signals consumed by delivery modes:
- `onDisconnectOrFailure(endpoint)` → used by `tryGetReply()`
- `permanentlyFailed(endpoint)` → used by `sendCanceler()` / `getReply()`
- `onFailedFor(endpoint, duration, slope)` → used by `getReplyUnlessFailedFor()`
- `onStateChanged(endpoint)` → used by `sendCanceler()` to re-evaluate

### Retry helpers

- `retryBrokenPromise(stream, req)` — loop `getReply()`, catch `broken_promise`, retry with jitter.
  For well-known endpoints (coordinators) that survive restarts.
- `brokenPromiseToNever(future)` — convert `broken_promise` to `Never()`.
  Used in worker registration to fall through to timeout branch.
- `retryGetReplyFromHostname(req, hostname, token)` — `tryGetReply()` + re-resolve DNS on failure.

---

## Layer 3: Load Balancer — Multi-Server Routing

**Files**: `LoadBalance.actor.h`, `MultiInterface.h`, `QueueModel.h`

This layer sits above fdbrpc and handles the case where **multiple servers can serve the same request** (e.g., multiple storage servers holding the same shard).

### MultiInterface\<T\>

A group of alternative servers for the same logical service.

```
MultiInterface<ReferencedInterface<StorageServerInterface>> {
    alternatives: Vec<(interface, distance)>  // sorted by locality distance
    countBest: usize  // number of alternatives in the same DC
}
```

Constructed from a list of interfaces + the client's locality. Alternatives are sorted
by distance: `SAME_DC < SAME_MACHINE < REMOTE`.

### ModelInterface\<T\> (probability-based subset of MultiInterface)

Used by GRV proxies. Maintains per-alternative probability weights, updated periodically
based on server busyness. Selection is weighted random via cumulative probability.

### QueueModel

Per-endpoint latency and queue depth tracking:
- `smoothOutstanding` — smoothed count of outstanding requests
- `latency` — smoothed request latency
- `penalty` — server-reported penalty (server says "I'm overloaded, avoid me")
- `failedUntil` — time until which this endpoint should be avoided

Fed by `ModelHolder` which tracks request start/end times and reports back.

### basicLoadBalance / loadBalance

The main entry point. Signature (LoadBalance.actor.h:690):

```
loadBalance(
    alternatives: MultiInterface,   // group of equivalent servers
    channel: member pointer,        // which RequestStream on the interface
    request: Request,
    atMostOnce: bool,               // propagate maybe_delivered vs retry
    model: QueueModel,              // optional latency model
)
```

Algorithm:
1. **Pick bestAlt**: server with lowest `smoothOutstanding` from QueueModel
   (or random if no model). Prefer local DC (`i < countBest`).
2. **Pick nextAlt**: second-best server for hedging.
3. **Send to bestAlt** via `tryGetReply()`.
4. **Hedge**: if bestAlt is slow, race a 2nd request to nextAlt after a delay
   computed from latency model (`secondMultiplier * nextTime + BASE_SECOND_REQUEST_TIME`).
   If bestAlt latency >> nextAlt, send immediately (`INSTANT_SECOND_REQUEST_MULTIPLIER`).
5. **On failure**:
   - `request_maybe_delivered` or `broken_promise`:
     - If `atMostOnce == true` → propagate to caller (don't retry, might have been processed)
     - If `atMostOnce == false` → retry on next alternative
   - `server_overloaded` → retry (return false from checkAndProcessResult)
   - `process_behind` → retry unless tried all options
   - Other errors → propagate to caller
6. **Cycle through alternatives** until success or all tried.
7. **Backoff** with `allAlternativesFailedDelay` if everything failed.

### AtMostOnce flag — the key design decision

This flag is how the **application layer** communicates intent to the load balancer:

- **`AtMostOnce::True`** (commits): "This request has side effects. If maybe-delivered,
  DON'T retry on another server — tell me so I can handle it."
  → Load balancer returns `request_maybe_delivered` to caller.

- **`AtMostOnce::False`** (reads): "This request is idempotent. If maybe-delivered,
  just try another server."
  → Load balancer silently retries on next alternative.

### TSS Comparison (bonus, probably not needed for backport)

When a storage server has a Testing Storage Server (TSS) pair, the load balancer
sends the request to both in parallel and compares results for consistency validation.

---

## Layer 4: Application — Choosing the Right API

The application never calls FlowTransport directly. It picks the right layer 2/3 API:

| Use case | API | Example |
|----------|-----|---------|
| Notification, no reply needed | `stream.send()` | heartbeat, trigger |
| Single known server, accept failure | `stream.tryGetReply()` | one-off probe |
| Single known server, must deliver | `stream.getReply()` | TLog rejoin |
| Single server, with failure timeout | `stream.getReplyUnlessFailedFor()` | singleton recruitment |
| Well-known endpoint, retry forever | `retryBrokenPromise(stream, req)` | coordinator RPC |
| Multiple equivalent servers, idempotent | `loadBalance(..., AtMostOnce::False)` | reads |
| Multiple equivalent servers, side effects | `loadBalance(..., AtMostOnce::True)` | commits |

Then adds domain-specific safety:
- Generation numbers for dedup (worker registration)
- Idempotency IDs for exactly-once (commits)
- Read-before-retry (commit with unknown result)
- Interface structs that bundle multiple `RequestStream`s into a typed API

---

## Backport Priority for Rust

### Must have (Layer 2 core)
1. `ReplyPromise<T>` — oneshot + endpoint token routing
2. `RequestStream<T>::send()` — fire-and-forget
3. `FailureMonitor` — at minimum: notify_disconnect, on_disconnect_or_failure
4. `RequestStream<T>::try_get_reply()` — needs FailureMonitor
5. `RequestStream<T>::get_reply()` — needs reliable send + sendCanceler
6. `RequestStream<T>::get_reply_unless_failed_for()` — needs onFailedFor

### Should have (Layer 2 helpers)
7. `retry_broken_promise()` — for well-known endpoints
8. `broken_promise_to_never()` — for registration patterns

### Nice to have (Layer 3)
9. `MultiInterface` — alternative server grouping with locality
10. `loadBalance()` — multi-server routing with hedging
11. `QueueModel` — latency-based server selection

Layer 3 can be deferred if you only have single-server RPCs initially.

---

## Key Insight

The entire stack is designed around one idea: **the network can lose your reply,
and different callers need different responses to that ambiguity.**

- Layer 1 gives you bytes + disconnect signal
- Layer 2 translates disconnect into `request_maybe_delivered`
- Layer 3 decides whether to retry on another server or propagate the ambiguity
- Layer 4 decides how to recover (generation dedup, idempotency ID, read-before-retry)

Each layer adds one decision. No layer tries to solve the whole problem.
