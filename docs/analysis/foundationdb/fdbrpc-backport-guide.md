# fdbrpc Backport Guide for Rust

## What fdbrpc is

fdbrpc is a thin layer between FlowTransport (raw bytes over TCP) and application code.
It adds **request/response pairing** and **delivery semantics** on top of a dumb transport.

FlowTransport gives you:
- `sendReliable(bytes, endpoint)` — queued, retransmitted on reconnect
- `sendUnreliable(bytes, endpoint)` — fire-and-forget, dropped on disconnect
- `peer.disconnect` — a signal when TCP drops
- `IFailureMonitor::notifyDisconnect(addr)` — broadcast failure

fdbrpc builds on this to give you `ReplyPromise<T>`, `RequestStream<T>`, and 3 delivery modes.

---

## Core Types to Implement

### 1. ReplyPromise\<T\>

**C++ location**: `fdbrpc.h:131-204`

A one-shot channel where the server sends back a response, routable over the network via an endpoint token.

```
ReplyPromise<T> = oneshot::Sender<Result<T, Error>> + Endpoint
```

Key behaviors:
- **Created by the client** as part of a request struct
- **Serialized** as just the endpoint token (UID) when sent over the network
- **On the server**: calling `reply.send(value)` serializes value and sends it to the endpoint
- **On the client**: the matching `Future<T>` resolves when the reply packet arrives
- **On drop without send**: delivers `broken_promise` error to the waiter

In Rust terms: `tokio::sync::oneshot` but the sender half can be serialized/deserialized
across the network. The endpoint token is the routing key.

### 2. RequestStream\<T\>

**C++ location**: `fdbrpc.h:726-835`

A handle to a remote (or local) actor's inbox. Wraps an endpoint.

```
RequestStream<T> = Endpoint + local_or_remote flag
```

Provides 3 methods with different delivery guarantees. These are the core of fdbrpc.

### 3. Endpoint

Already in FlowTransport. A `(NetworkAddress, Token)` pair where Token is a UID.
Well-known endpoints have stable tokens that survive process restarts.

---

## The 3 Delivery Modes

### Mode 1: `send()` — fire-and-forget

```
fdbrpc.h:730-738
```

```
fn send(&self, request: T) {
    if self.is_remote() {
        transport.send_unreliable(serialize(request), self.endpoint);
    } else {
        local_queue.send(request);
    }
}
```

- No reply expected
- Zero or one delivery
- Used for: heartbeats, notifications, triggers

### Mode 2: `try_get_reply()` — at-most-once with reply

```
fdbrpc.h:779-826
genericactors.actor.h:362-398  (waitValueOrSignal)
```

This is the interesting one. Pseudocode:

```
async fn try_get_reply(&self, request: T) -> Result<T::Reply, Error> {
    let disconnect = failure_monitor.on_disconnect_or_failure(self.endpoint);

    // Already known to be down?
    if disconnect.is_ready() {
        return Err(request_maybe_delivered());
    }

    // Send unreliably (dropped if connection fails)
    let peer = transport.send_unreliable(serialize(request), self.endpoint);

    // Race: reply vs disconnect
    select! {
        reply = request.reply_promise.recv() => Ok(reply),
        _ = disconnect => Err(request_maybe_delivered()),
        // On broken_promise (endpoint destroyed):
        //   mark endpoint as not found in failure monitor
        //   return Err(request_maybe_delivered())
    }
}
```

The `waitValueOrSignal` function (`genericactors.actor.h:362-398`) is the implementation.
It handles 3 cases:
1. Reply arrives -> return it
2. Disconnect signal fires -> return `request_maybe_delivered`
3. `broken_promise` (endpoint actor died) -> mark endpoint not found, same as disconnect

### Mode 3: `get_reply()` — at-least-once

```
fdbrpc.h:751-762
genericactors.actor.h:400-432  (sendCanceler)
```

```
async fn get_reply(&self, request: T) -> Result<T::Reply, Error> {
    // Send reliably (queued, retransmitted on reconnect)
    let reliable_handle = transport.send_reliable(serialize(request), self.endpoint);

    // Wait for reply, but cancel the reliable packet if endpoint is permanently dead
    loop {
        if failure_monitor.permanently_failed(self.endpoint) {
            transport.cancel_reliable(reliable_handle);
            // wait forever (caller will get broken_promise when promise is dropped)
            pending().await;
        }

        select! {
            reply = request.reply_promise.recv() => {
                transport.cancel_reliable(reliable_handle);
                return Ok(reply);
            }
            _ = failure_monitor.on_state_changed(self.endpoint) => {
                // Re-check if permanently failed
                continue;
            }
        }
    }
}
```

The `sendCanceler` function (`genericactors.actor.h:400-432`) is the implementation.
The reliable packet sits in `peer.reliable` queue. If the connection drops and reconnects,
FlowTransport automatically retransmits it. The server may receive the request **multiple times**.

---

## FailureMonitor

**C++ location**: `FailureMonitor.h`, `FailureMonitor.actor.cpp`

Two-level failure tracking:

```
FailureMonitor {
    // Address-level: is this machine/process reachable?
    address_status: HashMap<NetworkAddress, FailureStatus>,

    // Endpoint-level: does this specific endpoint exist?
    failed_endpoints: HashMap<Endpoint, FailureReason>,  // NOT_FOUND or UNAUTHORIZED
}
```

Key methods:
- `notify_disconnect(addr)` — called by FlowTransport when TCP drops
- `endpoint_not_found(endpoint)` — called when broken_promise received
- `on_disconnect_or_failure(endpoint) -> Future<()>` — fires when endpoint becomes unreachable
- `permanently_failed(endpoint) -> bool` — true if endpoint-level failure (not just address)
- `on_state_changed(endpoint) -> Future<()>` — fires on any status change

The failure monitor does NOT actively probe. It reacts to signals from FlowTransport
(connection drops) and from RPC responses (endpoint not found).

---

## How Packets Route Back to ReplyPromise

This is the key wiring:

1. Client creates `ReplyPromise<T>` which allocates a local endpoint token (UID)
2. Client registers this token in the local `EndpointMap` -> points to a `NetworkMessageReceiver`
3. The receiver is backed by a `NetSAV<T>` (network-aware single assignment variable = oneshot)
4. Request (including the reply endpoint token) is serialized and sent to server
5. Server deserializes, gets the reply endpoint token
6. Server does work, then calls `reply.send(value)` which serializes value and sends to client's endpoint
7. Client's FlowTransport receives packet, looks up token in EndpointMap, delivers to the NetSAV
8. The client's Future resolves

```
Client                          Network                         Server
------                          -------                         ------
ReplyPromise<T> created
  -> allocates endpoint token
  -> registers in EndpointMap

serialize(Request{..., reply_token})
  -> send to server endpoint    -------->    deserialize Request
                                             process request
                                             reply.send(value)
                                             serialize(value)
                                <--------    send to reply_token

FlowTransport receives packet
  -> scanPackets extracts token
  -> EndpointMap[token] -> NetSAV
  -> NetSAV.send(value)
  -> Future<T> resolves
```

---

## Retry Helpers (higher level, build after core works)

### retryBrokenPromise — for well-known endpoints

```
genericactors.actor.h:39-57
```

```
async fn retry_broken_promise<Req>(stream: &RequestStream<Req>, req: Req) -> Req::Reply {
    loop {
        match stream.get_reply(req.clone()).await {
            Ok(reply) => return reply,
            Err(e) if e == broken_promise => {
                req.reset_reply();  // allocate fresh ReplyPromise
                sleep_jittered(PREVENT_FAST_SPIN_DELAY).await;
            }
            Err(e) => return Err(e),
        }
    }
}
```

Only for well-known endpoints (coordinators, cluster controller) that restart at the same token.

### brokenPromiseToNever — suppress broken_promise

Used in worker registration (`worker.actor.cpp:695`): wraps a future so that `broken_promise`
becomes `Never` (pending forever), causing the caller to fall through to a timeout or
interface-change branch instead.

---

## Implementation Order

1. **ReplyPromise\<T\> + endpoint routing** — oneshot with network-routable token
2. **RequestStream\<T\>::send()** — fire-and-forget (simplest mode)
3. **FailureMonitor** — at minimum: `notify_disconnect`, `on_disconnect_or_failure`
4. **RequestStream\<T\>::try_get_reply()** — needs FailureMonitor for the disconnect signal
5. **RequestStream\<T\>::get_reply()** — needs reliable send + sendCanceler logic
6. **retryBrokenPromise** — simple loop, build when you need well-known endpoints

---

## Rust Mapping Cheat Sheet

| C++ (fdbrpc)                | Rust equivalent                          |
|-----------------------------|------------------------------------------|
| `ReplyPromise<T>`          | `oneshot::Sender<Result<T,E>>` + Endpoint |
| `Future<T>` (FDB)          | `oneshot::Receiver<Result<T,E>>`          |
| `RequestStream<T>`         | newtype around `Endpoint` + transport ref |
| `SAV<T>` / `NetSAV<T>`    | The oneshot channel internals             |
| `waitValueOrSignal`        | `tokio::select!` on reply vs disconnect   |
| `sendCanceler`             | `tokio::select!` on reply vs failure loop |
| `IFailureMonitor`          | Trait with `watch`-based notifications    |
| `EndpointMap`              | `HashMap<Token, Box<dyn MessageReceiver>>`|
| `broken_promise` on drop   | `impl Drop` that sends Err to receiver    |
| `ReliablePacket`           | Your transport's reliable send handle     |
| `Peer.disconnect`          | `tokio::sync::watch` or `broadcast`       |
