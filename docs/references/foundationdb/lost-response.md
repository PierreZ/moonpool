# FoundationDB Lost Response Handling: Research Summary

## Context

Research task — no code changes. Understanding how FoundationDB handles lost responses at the **fdbrpc layer** and **cluster controller**, and cataloging concrete strategies for modeling RPCs.

---

## The Fundamental Problem

When a server processes a request and sends a reply, but the TCP connection drops before the client receives it, the client faces **the maybe-delivered ambiguity**: it cannot distinguish "server never got my request" from "server processed it but the reply was lost."

FDB makes this ambiguity **explicit** via the `request_maybe_delivered` error code (1030), rather than hiding it behind a generic timeout.

```cpp
// flow/include/flow/error_definitions.h:57
ERROR( request_maybe_delivered, 1030, "Request may or may not have been delivered" )
```

---

## The Three Delivery Modes

`RequestStream<T>` (`fdbrpc/include/fdbrpc/fdbrpc.h:726-826`):

| Method | Guarantee | On Connection Drop |
|--------|-----------|-------------------|
| `.send()` | Fire-and-forget, at-most-once | Silently lost |
| `.tryGetReply()` | At-most-once with reply | Returns `request_maybe_delivered` |
| `.getReply()` | At-least-once (auto-retry) | Retransmits on reconnect; server may see duplicates |

**No per-message IDs at the RPC layer.** Tracking is via endpoint tokens (UIDs) + Promise reference counting.

### `.send()` — Fire-and-Forget (line 733)

```cpp
void send(U&& value) const {
    if (queue->isRemoteEndpoint()) {
        FlowTransport::transport().sendUnreliable(
            SerializeSource<T>(std::forward<U>(value)), getEndpoint(), true);
    } else
        queue->send(std::forward<U>(value));
}
```

### `.getReply()` — At-Least-Once (line 752)

Uses `sendCanceler` (`genericactors.actor.h:400-431`) which retransmits via `sendReliable` and monitors endpoint failure state. On `broken_promise`, marks the endpoint as not found and keeps the reliable packet queued for retransmission on reconnect.

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

### `.tryGetReply()` — At-Most-Once (line 785)

Uses `waitValueOrSignal` (`genericactors.actor.h:362-398`) which races the reply future against a disconnection signal. On disconnect, returns `request_maybe_delivered`. On `broken_promise`, also converts to `request_maybe_delivered`.

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

---

## 6 Concrete Strategies for Modeling RPCs

### Strategy 1: Idempotent-by-Design ("Set State, Don't Mutate")

**Principle**: Design the request to describe the **desired end state**, not a delta. Re-delivery is harmless.

**FDB Examples**:
- **Worker registration** — "I am worker X with these capabilities" (not "add me to the worker list")
- **TLog rejoin** (`fdbserver/TLogServer.actor.cpp:~2599`) — "TLog X is alive and wants to rejoin" (re-sending is harmless)
- **Master registration** — "Master X with this config exists" (idempotent assertion)

**Counter example**: Instead of "increment counter", model it as "set counter to N" where the client tracks the expected value.

**When to use**: Whenever you can reformulate the operation as a state assertion.

### Strategy 2: Generation / Sequence Number Dedup

**Principle**: Tag each request with a monotonic number. Server tracks the last-seen number per client and skips duplicates.

**FDB Examples**:

**RegisterWorkerRequest.generation** — Worker declares `state Generation requestGeneration = 0` (`worker.actor.cpp:615`) and increments it on each registration loop iteration (`worker.actor.cpp:639`):

```cpp
RegisterWorkerRequest request(
    interf, initialClass, processClass, asyncPriorityInfo->get(),
    requestGeneration++,  // monotonically increasing
    ddInterf->get(), rkInterf->get(), /* ... */);
```

The CC checks `req.generation >= info->second.gen` (`ClusterController.actor.cpp:1292`). Old generations are ignored.

**RegisterMasterRequest.registrationCount** — CC checks `req.registrationCount <= db->masterRegistrationCount` (`ClusterController.actor.cpp:1058`) to skip stale registrations.

**Counter example**: Client sends `{clientId, seq: 5, op: increment}`. Server stores `lastSeq[clientId]`. If `seq <= lastSeq`, return cached result. Otherwise, increment and cache.

**When to use**: Non-idempotent operations where the client initiates retries. Requires server-side per-client state.

### Strategy 3: Fire-and-Forget (Accept Loss)

**Principle**: Use `.send()` for RPCs where losing the message is tolerable. No reply expected.

**FDB Examples**:
- **waitFailure** — Server holds the request open until it dies; losing the request just means the client re-sends. Every server interface has `RequestStream<ReplyPromise<Void>> waitFailure`.
- **Heartbeats** — Missing one is fine; the next one compensates.
- **Notifications/triggers** — `updateDBInfo.trigger()`. Redundant triggers are harmless.

**When to use**: Probes, heartbeats, advisory data, notifications.

### Strategy 4: Retry-then-Check (Read-Before-Retry)

**Principle**: On `request_maybe_delivered`, read the server's state to determine if the previous request succeeded before retrying.

**FDB Example — Commit with idempotency ID** (`NativeAPI.actor.cpp:6829-6866`):

On `commit_unknown_result` or `request_maybe_delivered`, if the transaction has an idempotency ID:
1. Commit a dummy conflicting transaction to ensure the original is no longer in flight
2. Call `determineCommitStatus()` to read the idempotency ID from system keyspace
3. If found → return cached commit version (the commit succeeded)
4. If not found → throw `transaction_too_old` (the commit didn't happen)

```cpp
if (e.code() == error_code_request_maybe_delivered || e.code() == error_code_commit_unknown_result) {
    // ...
    if (req.idempotencyId.valid()) {
        Optional<CommitResult> commitResult = wait(determineCommitStatus(
            trState, req.transaction.read_snapshot,
            req.transaction.read_snapshot + CLIENT_KNOBS->MAX_WRITE_TRANSACTION_LIFE_VERSIONS,
            req.idempotencyId));
        if (commitResult.present()) {
            // Commit succeeded — return cached version
            CODE_PROBE(true, "AutomaticIdempotencyCommitted");
            return Void();
        } else {
            CODE_PROBE(true, "AutomaticIdempotencyNotCommitted");
            throw transaction_too_old();
        }
    }
    throw commit_unknown_result();
}
```

**When to use**: When the operation has observable side effects that can be queried, and you need exactly-once semantics.

### Strategy 5: Well-Known Endpoint + retryBrokenPromise

**Principle**: For endpoints that survive process restarts (well-known tokens), catch `broken_promise` and retry indefinitely with jitter.

**Code** (`genericactors.actor.h:39-57`):
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

**FDB Examples**:
- **Coordinator protocol info** — `retryBrokenPromise(requestStream, ProtocolInfoRequest{})`
- **Leader election** — Coordinator GetLeader requests
- **Config transactions** — `PaxosConfigTransaction`

**When to use**: Singleton services with stable addresses (coordinators, cluster controller). NOT for ephemeral endpoints.

### Strategy 6: Load-Balanced with AtMostOnce Flag

**Principle**: Use the load balancer to try alternative servers. The `AtMostOnce` flag controls whether `request_maybe_delivered` triggers a retry on another server or propagates to the caller.

**Code** (`LoadBalance.actor.h:591-618`):
```cpp
bool maybeDelivered = errCode == error_code_broken_promise ||
                      errCode == error_code_request_maybe_delivered;
// ...
if (atMostOnce && maybeDelivered) {
    return request_maybe_delivered();  // Don't retry — might have been processed
}
```

**FDB Examples**:
- **Reads** — Use `AtMostOnce::False` (`NativeAPI.actor.cpp:132`) because reads are inherently idempotent; retrying on another storage server is safe.
- **Commits** — The commit path catches `request_maybe_delivered` and converts it to `commit_unknown_result`, which propagates to the client layer. The client can then use Strategy 4 (idempotency IDs) for exactly-once semantics.

**When to use**: Operations with multiple equivalent servers. Use `AtMostOnce::True` for non-idempotent ops, `False` for idempotent ones.

---

## Decision Flowchart

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
  YES -> Strategy 6: load balance with AtMostOnce::True + handle error at caller
  NO  -> Strategy 5 (if well-known) or Strategy 2 (add server state)
```

---

## Comparison with Orleans

| Aspect | FoundationDB | Orleans |
|--------|-------------|---------|
| **Failure signal** | Explicit `request_maybe_delivered` | Generic timeout exception |
| **Delivery modes** | 3 choices per call site | At-most-once only |
| **Retry policy** | Caller chooses strategy per RPC | Caller's responsibility, no framework support |
| **Dedup support** | Generation numbers, idempotency IDs (app-level) | Single-threaded grain activation (natural serialization, but no dedup) |
| **Philosophy** | Expose ambiguity, let each layer decide | Hide ambiguity behind timeout |

**Key difference**: FDB gives you the vocabulary (`maybe_delivered`, `broken_promise`, `getReply` vs `tryGetReply`, `AtMostOnce` flag) to express your intent precisely. Orleans gives you one tool (timeout) and hopes the grain's single-threaded nature makes it less painful.

---

## Key Files

| Component | Path |
|-----------|------|
| 3 delivery modes | `fdbrpc/include/fdbrpc/fdbrpc.h:726-826` |
| waitValueOrSignal (tryGetReply impl) | `fdbrpc/include/fdbrpc/genericactors.actor.h:362-398` |
| sendCanceler (getReply impl) | `fdbrpc/include/fdbrpc/genericactors.actor.h:400-431` |
| retryBrokenPromise | `fdbrpc/include/fdbrpc/genericactors.actor.h:39-57` |
| Load balancer + AtMostOnce | `fdbrpc/include/fdbrpc/LoadBalance.actor.h:583-625` |
| Error definitions | `flow/include/flow/error_definitions.h` |
| Worker registration (Strategy 2) | `fdbserver/worker.actor.cpp:615-651` |
| CC generation dedup (Strategy 2) | `fdbserver/ClusterController.actor.cpp:1290-1352` |
| Commit idempotency (Strategy 4) | `fdbclient/NativeAPI.actor.cpp:6829-6871` |
| TLog rejoin (Strategy 1) | `fdbserver/TLogServer.actor.cpp:~2599` |
