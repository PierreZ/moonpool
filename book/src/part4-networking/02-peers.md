# Peers and Connections

<!-- toc -->

A raw TCP connection is fragile. It can drop at any moment, and when it does, every in-flight message is lost. Our distributed system needs something more resilient: a logical connection that survives transient network failures. That is the `Peer`.

## The Peer Abstraction

A `Peer` represents a **logical connection** to a remote endpoint. Behind the scenes it manages the actual TCP stream, but from the caller's perspective it provides a simple interface: queue a message, and the peer will deliver it. If the connection drops, the peer reconnects automatically and drains any queued messages once the link is back.

This follows FoundationDB's `FlowTransport` pattern (`FlowTransport.actor.cpp:1016-1125`). FDB's Peer is an actor that owns a connection and handles reconnection internally. Ours works the same way, using a background task spawned via `TaskProvider`.

Creating a peer is straightforward:

```rust
let peer = Peer::new(
    providers.clone(),
    "10.0.1.2:4500".to_string(),
    PeerConfig::default(),
);
```

The peer immediately spawns a `connection_task` that attempts to connect to the destination. Once connected, it enters a loop: read incoming packets, write outgoing packets, and handle failures.

## Message Queuing During Disconnection

When the connection is down, outgoing messages do not vanish. The peer maintains two internal queues:

- **Reliable queue**: Messages that must survive disconnection. When the connection drops, these are preserved and sent first after reconnection.
- **Unreliable queue**: Messages that can be dropped on failure. These are drained after the reliable queue.

Both queues are bounded by `PeerConfig::max_queue_size` (default: 1000). If the queue fills up while disconnected, new messages are dropped and the caller gets a `PeerError::QueueFull` error. This prevents unbounded memory growth during long outages.

## Connection Lifecycle

The peer's background task follows a clear lifecycle:

```text
┌──────────┐    connect     ┌───────────┐
│Disconnect├───────────────►│ Connected │
│   ed     │◄───────────────┤           │
└────┬─────┘  conn. error   └─────┬─────┘
     │                            │
     │ backoff                    │ send/recv
     ▼                            ▼
┌──────────┐              ┌───────────┐
│Reconnect │              │   Active  │
│  ing     │              │   I/O     │
└──────────┘              └───────────┘
```

On successful connection, the task enters the Active I/O phase. It reads incoming packets (dispatching them via a channel) and writes outgoing packets (draining the queues). If an I/O error occurs, it transitions to Disconnected, waits through backoff, then tries again.

For **incoming** connections (accepted by a server listener), the peer starts already connected. The constructor `Peer::new_incoming()` takes an existing TCP stream and skips the initial connection attempt. If this connection drops, the peer **exits** rather than reconnecting, because the remote side is responsible for initiating a new connection.

## Health Monitoring with Ping/Pong

A TCP connection can appear alive while the remote process is actually unresponsive. To detect this, outbound peers run a **ping/pong protocol** modeled on FDB's `connectionMonitor` (`FlowTransport.actor.cpp:616-699`).

The `PingTracker` state machine works like this:

1. After each `ping_interval` (default: 1 second), send a PING packet
2. Wait up to `ping_timeout` (default: 2 seconds) for a PONG reply
3. If PONG arrives, record the RTT and return to idle
4. If timeout but bytes were received since the ping, **tolerate** it (the connection is busy, not dead)
5. If timeout and no bytes were received, or if `max_tolerated_timeouts` consecutive tolerations occur, **tear down** the connection

Ping and pong packets use special wire tokens (`PING_TOKEN` and `PONG_TOKEN`) that are intercepted by the connection task and never delivered to the application layer.

This monitoring runs only on outbound peers. Incoming peers passively respond to pings but never initiate them.

## Under Simulation

In simulation, peers experience chaos:

- Random connection closes (0.001% probability)
- Connection failures (50% probabilistic during buggify)
- Partial writes
- Half-open connection simulation

The peer does not know it is running in simulation. It sees the same `connect()` failures and `read()` errors that would occur with a flaky real network. Its reconnection logic, backoff timing, and queue management are all exercised against these faults using the same code paths that run in production.
