# Simulating the Network

<!-- toc -->

When we built the provider traits in Part 2, we abstracted network I/O behind `NetworkProvider`. Now we need to decide **what** to simulate. This choice shapes everything that follows.

## TCP, Not Packets

Many network simulators model individual packets: MTU fragmentation, congestion windows, selective acknowledgments, out-of-order delivery at the segment level. That approach is powerful for protocol research, but it is the wrong tool for testing distributed systems.

Our systems speak TCP. They open connections, send messages, read responses, and handle disconnections. The bugs that kill production clusters happen at **connection** granularity, not packet granularity:

- A node loses its connection mid-write and the remote sees a partial message
- A connection hangs in half-open state where one side thinks it is alive and the other does not
- Three nodes reconnect simultaneously after a network partition and overwhelm each other
- A process reboots and all its peers attempt reconnection at the same moment

These failure modes do not require modeling individual packet routing. They require modeling **connection lifecycle**: establishment, latency, graceful close, abrupt close, and half-open states.

FoundationDB reached the same conclusion. Their `FlowTransport` layer (the inspiration for our `moonpool-transport` crate) simulates connections, not packets. Their `sim2.actor.cpp` provides TCP-like streams that can be delayed, corrupted, or severed. Individual packet routing, MTU sizes, and congestion control are left to the real kernel.

## What We Simulate

The simulation network provides:

- **Connection establishment** with configurable latency. A `connect()` call goes through the simulated network, introducing delays that the chaos engine can stretch or shorten.
- **Reliable byte streams** that behave like TCP. Once connected, reads and writes operate on a stream of bytes, not discrete packets. The wire format handles framing.
- **Connection failures** injected by the chaos engine. Connections can be dropped at any point, forcing reconnection logic to activate.
- **Half-open connections** where one side believes the connection is alive while the other has closed it. This requires the FIN delivery mechanism we built into the simulation.
- **Latency injection** on data delivery events. Messages are not delivered instantly between simulated processes. They sit in the event queue with configurable delays.

## What We Do Not Simulate

We deliberately skip:

- **Individual packet routing** between simulated hosts
- **MTU and fragmentation** at the IP layer
- **Congestion windows and flow control** (TCP slow start, etc.)
- **DNS resolution**
- **TLS handshakes** (the simulation trusts all connections)

These are real concerns in production, but they are not where distributed system bugs hide. A consensus protocol that breaks under packet reordering has a design flaw, not a testing gap.

## Same Code, Two Worlds

The key architectural property: **the same application code runs in both environments**. A server process that opens connections, sends RPC requests, and handles failures uses the `Providers` trait bundle. In production, that bundle contains `TokioNetworkProvider` backed by real TCP sockets. In simulation, it contains the simulated network where every connection goes through the `SimWorld` event queue.

```text
Application Code
     │
     ▼
┌─────────────┐
│  Providers   │ ◄── trait bundle
├─────────────┤
│ NetworkProv. │ ◄── connect(), bind(), accept()
└──────┬──────┘
       │
  ┌────┴─────┐
  │          │
  ▼          ▼
Real TCP   SimWorld
(tokio)    (event queue)
```

There is no `#[cfg(test)]` branching. No mock objects. The transport layer, the peer abstraction, the RPC system, and the wire format are all production code that happens to run inside a deterministic simulation.

This is what makes the approach work. We are not testing a simplified model of our networking. We are testing the actual networking code against a hostile simulated environment that is **worse** than production.
