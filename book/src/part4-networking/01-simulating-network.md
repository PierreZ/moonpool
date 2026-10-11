# Simulating the Network

<!-- toc -->

Part II put TCP behind `NetworkProvider`. In production, `TokioNetworkProvider`
hands the application real sockets. In simulation, the same calls hand it
provider-backed listeners and a `SimTcpStream`. Both implement
`futures::io::AsyncRead` and `AsyncWrite`, so the application sees one
interface in both worlds.

That seam is small on purpose. Moonpool simulates what a distributed
application can observe of its network. It does not pretend to be an IP
stack.

## TCP, Not Packets

Many network simulators model packets: MTU fragmentation, congestion windows,
selective acknowledgements, routing. That fidelity helps protocol research.
It is wasted on an application whose contract starts at a TCP byte stream.

Our systems open connections, write messages, read replies and handle
disconnects. The bugs that take a cluster down live at **connection**
granularity:

- A node loses its connection mid-write and the peer reads half a message.
- A one-way partition lets one side hear heartbeats while its replies vanish.
- A process reboots while clients still hold streams to the old boot.
- A partition heals after several request deadlines have passed.

These failures need connection setup, byte delivery, partial I/O, latency and
close semantics. They do not need a simulated router. FoundationDB drew the
same line in `sim2.actor.cpp`: simulated connections can be delayed,
corrupted or cut, and MTU discovery stays with the kernel. Moonpool draws it
at the provider.

## What We Simulate

The network engine controls:

- **Connect, bind and accept.** Each one is a delayed operation. It stays
  pending until its scheduler event fires. A listener has a backlog, and a
  handshake can succeed, fail or hang under chaos.
- **Byte delivery.** A write enters the connection's send buffer, and the
  engine asks the global scheduler for ordered delivery events. Reads and
  writes can be partial, which catches loops that assume one poll moves one
  whole message.
- **Latency, clogs and partitions.** Per-link latency, temporary clogs and
  partitions decide when, or whether, bytes arrive.
- **Corruption.** A bit flip in flight makes checksum paths reachable. A raw
  protocol must bring its own integrity check.
- **Connection lifecycle.** A graceful close delivers buffered bytes before
  EOF. An abrupt close shows as an I/O error or as a silent peer.

Every decision comes from the seeded RNG. A connection that hangs on seed 73
hangs the same way on every replay of seed 73. The fault families are listed
in [Network Faults](../part3-building/10-network-faults.md): `Clog`,
`Partition`, `BitFlip`, `RandomClose`, `ConnectFailure`, `ClockDrift`,
`BuggifiedDelay`, `PairLatency` and `BlackHole`, each one maskable per seed
with `network_fault_mask`.

## Who Owns What

The provider is a thin adapter over `NetworkSimulation`. That engine owns
listeners, connection pairs, backlogs, topology, partition state, faults,
delayed-operation results and every network waker. It owns no clock and no
private queue.

`SimWorld` runs one `Scheduler<Event>` for timers, network events, storage
events and process lifecycle. A network transition returns ordered actions,
such as "schedule a delivery" or "cancel this operation". The coordinator
applies them, releases its lock, and only then wakes tasks. A waker that
polls network state again never deadlocks, because no wake happens under the
lock.

Endpoints are owned. A `bind` holds its address until the listener drops or
its process dies. A second `bind` of a live address fails with `AddrInUse`.
A `connect` to an address nobody listens on fails with `ConnectionRefused`.
Startup, shutdown and misconfiguration therefore behave as they do against
the kernel. Port zero is ephemeral and never collides.

Delayed bind and connect futures are cancellation-safe. An `accept` reserves
one backlogged connection while its latency elapses. Dropping the future
returns the reservation to the front of the backlog. Shutdown fails every
delayed operation and drains the waiters instead of parking them.

## A Raw TCP Process

The server below is ordinary process code. It binds its assigned address,
reads one fixed-size request and echoes it. `read_exact` matters: the network
may split one delivery into several reads.

```rust,ignore
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_sim::{NetworkProvider, TcpListenerTrait};

let listener = ctx.network().bind(ctx.my_ip()).await?;
let (mut stream, peer) = listener.accept().await?;

let mut frame = [0_u8; 16];
stream.read_exact(&mut frame).await?;
tracing::info!(%peer, "request_received");
stream.write_all(&frame).await?;
```

The client uses the same boundary:

```rust,ignore
let mut stream = ctx.network().connect(&server_ip).await?;
stream.write_all(&request_frame).await?;
stream.read_exact(&mut response_frame).await?;
```

No simulation-only socket API appears on either side. Generic code takes a
`P: Providers` or an `N: NetworkProvider` and runs over `TokioProviders` in
production. Inside a `Process` or a `Workload`, `SimContext` supplies the
simulated implementation.

Your protocol still owns framing, checksums, retries and deadlines. That is
the point: the simulator exercises the code you ship, including its behaviour
when a frame is truncated or a retry is ambiguous. For hyper, axum and tonic,
[`moonpool-hyper`](../part4-integration/08-hyper-stack.md) adapts the same
provider streams. For a typed request/reply layer written against these
traits, see [Typed RPC with moonpool-rpc](./02-rpc.md).

## What We Do Not Simulate

- Packet routing between hosts
- MTU and IP fragmentation
- Congestion windows and slow start
- DNS resolution (hostnames reach the simulation only through a
  `moonpool_core::Resolver` the application supplies)
- TLS handshakes inside the network engine
- Nagle's algorithm and delayed ACKs: a simulated write is delivered after
  its scheduled latency and is never held back for an earlier segment's
  acknowledgement

These need integration and production testing. The simulation spends its
effort where seeded replay pays most: application-visible timing, failure
and byte-stream behaviour.

## Same Code, Two Worlds

`TokioNetworkProvider` sets `TCP_NODELAY` on every stream it connects or
accepts, as FoundationDB's `Net2` does. Request/reply protocols write a small
frame and wait for its answer. With Nagle on, that frame can wait for a
delayed ACK, which adds latency the simulator never shows. With it off,
production and simulation agree on this point. The setting is best-effort: a
platform that refuses it leaves the connection as it is.

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
(tokio)    (Scheduler<Event>)
                │
                ▼
         NetworkSimulation
```

There is no `#[cfg(test)]` branch and no socket mock in the application. The
parser, the request handling, the retry policy and the timeouts are the
shipped code, run against a hostile deterministic network. Production swaps
the provider and hands those same functions real sockets. That rule has a
chapter of its own:
[Same Code in Production and Simulation](../part2-foundations/12-same-code.md).
