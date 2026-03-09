# Failure Monitor

<!-- toc -->

Delivery modes need a way to detect when a remote endpoint is unreachable. Polling for liveness would be wasteful and nondeterministic. Instead, moonpool uses a **reactive failure monitor** that tracks connection state and wakes interested futures when something changes. This follows FoundationDB's `SimpleFailureMonitor` (`FailureMonitor.h:146`).

## Two Levels of Tracking

The `FailureMonitor` tracks failures at two granularities:

**Address-level**: Is this machine reachable? The connection task calls `set_status(address, Available)` on successful connect and `notify_disconnect(address)` when the TCP link drops. Unknown addresses default to `Failed`, a conservative assumption that prevents sending requests into the void.

**Endpoint-level**: Is this specific endpoint permanently gone? When a `ReplyFuture` receives `BrokenPromise` (the server dropped the promise without responding), the delivery mode calls `endpoint_not_found(endpoint)`. This marks the endpoint as permanently failed. Well-known endpoints (system tokens like `Ping`) are exempt from permanent failure.

```rust
// Producer side (connection_task)
failure_monitor.set_status("10.0.1.2:4500", FailureStatus::Available);
// ... later, on TCP drop:
failure_monitor.set_status("10.0.1.2:4500", FailureStatus::Failed);
failure_monitor.notify_disconnect("10.0.1.2:4500");
```

## Reactive, Not Polling

The failure monitor never probes. It reacts to signals from the connection layer and wakes registered consumers. The consumer API returns futures that resolve when state changes:

| Method | Resolves when | Used by |
|--------|--------------|---------|
| `on_disconnect_or_failure(endpoint)` | Address disconnects OR endpoint permanently fails | `try_get_reply()` |
| `on_disconnect(address)` | Address disconnects | Connection monitoring |
| `on_state_changed(endpoint)` | Any status change (never resolves if permanently failed) | `get_reply()` retry loop |
| `state(endpoint)` | Immediate check | Fast-path in `try_get_reply()` |

All of these use `Waker`-based registration internally. When the producer calls `set_status` or `notify_disconnect`, it drains the waker list for that address and wakes every registered consumer. No background tasks, no timers, no allocation beyond the waker vector.

## How Delivery Modes Use It

The connection between delivery modes and the failure monitor is the core of the RPC stack.

**`try_get_reply`** races the reply against `on_disconnect_or_failure`. If the connection drops before the server responds, the future resolves with `MaybeDelivered`. Before even sending, it checks `state(endpoint)` for the fast path: if the endpoint is already failed, return `MaybeDelivered` immediately without wasting a network round-trip.

**`get_reply_unless_failed_for`** first waits on `on_disconnect_or_failure`, then sleeps for the sustained failure duration via the `TimeProvider`. If the connection recovers during the sleep window, the reliable retransmit resolves the reply future first, winning the `select!` race. If not, the caller gets `MaybeDelivered`.

**`get_reply`** does not directly use the failure monitor for cancellation. It relies on the reliable queue and the 30-second RPC timeout. But `BrokenPromise` responses trigger `endpoint_not_found`, feeding information back into the monitor for future requests.

## Under Simulation

In simulation, the failure monitor works identically. The `SimWorld` triggers the same `set_status` and `notify_disconnect` calls through simulated connection events. This means delivery mode behavior under chaos is fully exercised: connection drops trigger `MaybeDelivered`, address recovery wakes pending futures, and permanently failed endpoints are tracked correctly.

The monitor caps permanently failed endpoints at 100,000 entries to prevent memory growth in long-running simulations with many ephemeral endpoints. When the cap is hit, the entire map is cleared and a warning is logged.
