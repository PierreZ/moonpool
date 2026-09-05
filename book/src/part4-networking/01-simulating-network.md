# Simulating the Network

<!-- toc -->

When we built the provider traits in Part II, we put TCP behind
`NetworkProvider`. Production gets real sockets from `TokioNetworkProvider`.
Simulation gets provider-backed listeners and `SimTcpStream`. Application code
sees the same `futures::io::AsyncRead` and `AsyncWrite` interface in both
places.

That seam is deliberately small. Moonpool simulates the network behavior a
distributed application can observe without pretending to be an IP stack.

## TCP, Not Packets

Many network simulators model individual packets: MTU fragmentation,
congestion windows, selective acknowledgments, and routing at the segment
level. That fidelity is useful for protocol research. It is usually wasted on
an application whose contract begins at a TCP byte stream.

Our systems speak TCP. They open connections, send messages, read responses, and handle disconnections. The bugs that kill production clusters happen at **connection** granularity, not packet granularity:

- A node loses its connection mid-write and the remote sees a partial message
- A one-way partition lets one side hear heartbeats while its replies disappear
- A process reboots while clients still hold streams to the old process
- A partition heals after several request deadlines have expired

These failure modes need connection establishment, byte delivery, partial I/O,
latency, and close semantics. They do not need a simulated router.

FoundationDB reached the same boundary in `sim2.actor.cpp`: simulated
connections can be delayed, corrupted, or severed, while MTU discovery and
congestion control stay with the real kernel. Moonpool follows that boundary at
the provider level. What runs above the stream is your code. It can be a small
binary protocol, hyper, axum, tonic, or something entirely private to your
system.

## What We Simulate

The simulation network controls:

- **Connect, bind, and accept.** Each call is a real delayed operation. It
  remains pending until its targeted scheduler event fires. A simulated
  listener has a backlog and a connection handshake that can succeed, fail, or
  hang under chaos.
- **Byte delivery.** Writes enter the connection's send buffer and the network
  engine requests ordered delivery events from the global scheduler.
  Partial writes and partial reads exercise loops that incorrectly assume one
  poll transfers a complete application message.
- **Latency and clogging.** Per-link latency, temporary clogs, and partitions
  change when or whether bytes arrive.
- **Corruption.** Bit flips make checksum and validation paths reachable. Raw
  protocols must provide their own integrity checks when corruption matters.
- **Connection lifecycle.** Graceful close delivers buffered bytes before EOF.
  Abrupt close can surface as an explicit I/O error or a silent peer failure.

Every decision comes from the seeded simulation RNG. A connection that hangs
on seed 73 hangs the same way every time seed 73 is replayed.

## Who Owns What

The networking provider is a thin adapter into `NetworkSimulation`. That
engine owns listeners, connection pairs, pending backlogs, topology, partition
state, network faults, delayed-operation results, and every network waker. It
does **not** own logical time or a private queue.

`SimWorld` coordinates a single `Scheduler<Event>` for timers, network events,
storage events, and process lifecycle. A network transition returns ordered
actions such as scheduling a `Delivery`, cancelling an operation, or
recording a fault. The coordinator applies those actions, releases its lock,
then wakes tasks. Keeping wake calls outside the lock lets a re-entrant waker
poll network state without deadlocking.

Delayed bind and connect futures are cancellation-safe. Accept reserves one
backlogged connection while its latency elapses, and dropping the future
returns that reservation to the front of the backlog. Shutdown fails delayed
operations and drains network waiters instead of leaving them parked.

## A Raw TCP Process

The server below is ordinary process code. It binds its assigned address, reads
one fixed-size request, and echoes it. `read_exact` matters because the network
is allowed to split delivery into smaller reads.

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

The client uses the same provider boundary:

```rust,ignore
let mut stream = ctx.network().connect(&server_ip).await?;
stream.write_all(&request_frame).await?;
stream.read_exact(&mut response_frame).await?;
```

No simulation-only socket API appears in either side. In generic application
code, accept a `P: Providers` or `N: NetworkProvider` and the same functions can
run over `TokioProviders` in production. Inside a `Process` or `Workload`,
`SimContext` supplies the simulated implementation.

Your protocol still owns framing, checksums, retries, and deadlines. This is a
feature, not missing infrastructure. The simulator should exercise the code you
ship, including its behavior when a frame is truncated or a retry is
ambiguous. For ecosystem protocols, `moonpool-hyper` provides the adapters that
let hyper and tonic consume the same provider streams.

## What We Do Not Simulate

Moonpool deliberately skips:

- **Individual packet routing** between simulated hosts
- **MTU and fragmentation** at the IP layer
- **Congestion windows and flow control** (TCP slow start, etc.)
- **DNS resolution**
- **TLS handshakes** inside `SimNetwork`

Those concerns still need integration and production testing. The simulation
focuses on application-visible timing, failure, and byte-stream behavior where
seeded replay gives the most leverage.

## Same Code, Two Worlds

The key architectural property is that **the same application code runs in both
environments**. Production uses real TCP sockets. Simulation routes each
connection and delivery through `SimWorld`.

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
protocol parser, request handling, retry policy, and timeout logic are the real
code, running against a hostile deterministic network. Then production swaps
the provider and hands those same functions real sockets.
