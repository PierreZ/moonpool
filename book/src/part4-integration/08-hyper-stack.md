# The hyper Stack: gRPC and HTTP
<!-- toc -->

The axum chapter put a web service into simulation by handing hyper a
simulated stream. That works because HTTP/1 asks almost nothing of the
runtime: parse bytes, write bytes, done. HTTP/2 is different. It multiplexes
streams over one connection, it pings the peer to notice death, and it runs
per-request work concurrently. To do that, hyper needs to **spawn tasks** and
**read a clock**.

hyper is runtime-agnostic about both. It defines `rt::Executor` and `rt::Timer`
traits and asks you to supply them. In production everyone supplies
`hyper-util`'s tokio versions, which is why the ecosystem looks tokio-only.
Inside a simulation those are the two things we cannot allow. A tokio spawn
never runs on the deterministic executor, and a tokio sleep runs on a wall
clock that no seed controls.

`moonpool-hyper` supplies the same hooks over the provider traits. It depends
only on `moonpool-core`, so it brings no production providers and no
simulation of its own. It is generic over `P: Providers`, and the facade
exposes it as `moonpool::hyper` behind the `hyper` feature.

| Type | What hyper asks for | What it uses |
|------|---------------------|--------------|
| `HyperExecutor<T>` | `rt::Executor`, spawn internal tasks | `TaskProvider` |
| `HyperTimer<T>` | `rt::Timer`, sleeps and `now()` | `TimeProvider` |
| `HyperIo<S>` | `rt::Read` + `rt::Write` | any `NetworkProvider` stream |
| `TowerToHyperService<S>` | a hyper `Service` | your tower service |
| `KeepAlive` | h2 PING settings | shared by both sides |
| `H2Server<P>` | (server plumbing) | the whole provider bundle |
| `ReconnectingChannel<P, B>` | (client plumbing) | the whole provider bundle |

The first five are thin. The last two are where the plumbing that every hyper
user writes by hand gets written once. Two cargo features, `client` and
`server`, both on by default, select them. With both off the crate still
builds the adapters and needs no hyper feature at all.

## Serving: One Call Per Connection

Here is the gRPC server from
[`tonic_grpc.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim-examples/src/tonic_grpc.rs):

```rust
let server = H2Server::new(ctx.providers()).with_config(H2ServerConfig {
    keep_alive: Some(keep_alive()),
    vectored_writes: true,
    ..H2ServerConfig::default()
});

loop {
    moonpool_sim::select! {
        accept = listener.accept() => {
            let (stream, _addr) = accept?;

            // A graceful Attrition reboot cancels the token. hyper then
            // finishes the RPCs in flight before the connection ends.
            let shutdown = ctx.shutdown().clone();
            let connection = server.serve_connection_with_shutdown(
                stream,
                echo.clone(),
                shutdown.clone().cancelled_owned(),
            );

            ctx.task()
                .spawn_task("grpc-server-conn", async move { connection.await })
                .detach();
        }
        () = ctx.shutdown().cancelled() => return Ok(()),
    }
}
```

`echo` is the generated `EchoServer`, an ordinary tower service.
`serve_connection_with_shutdown` wraps it in `TowerToHyperService`, builds the
`HyperIo`, and returns a future that is `Send + 'static`. It spawns as a normal
provider task. h2 connection futures are `Send`, unlike hyper's HTTP/1 state
machine, which is why this is a spawn and not the inline `FuturesUnordered`
loop of the axum example. `serve_connection` without a shutdown future does the
same with no drain.

The **accept loop stays with the caller**. The race between a new connection
and progress on existing ones is a real ordering question. Under the seeded
`select!` different seeds explore both answers. A helper that hid it would hide
a property worth testing.

`H2ServerConfig` has three fields:

| Field | Default | Meaning |
|-------|---------|---------|
| `keep_alive` | `None` | h2 PING interval and timeout. `KeepAlive::while_idle` is a client knob and is ignored here |
| `vectored_writes` | `false` | what `HyperIo` reports to hyper. `true` makes the sim apply chaos per `IoSlice` |
| `drain_timeout` | `Some(DEFAULT_DRAIN_TIMEOUT)`, 30 s | how long a graceful drain may run once shutdown is requested |

The drain is bounded. A peer that stops reading cannot hold the connection
future open forever. When the deadline runs out the connection is dropped and
the future resolves to `ServeError::DrainTimedOut`. Any other bad end is
`ServeError::Connection`, which wraps hyper's error. Under chaos that is an
expected outcome, not a bug. The example checks that the drain really happens
with `assert_sometimes!(true, "grpc_server_drained_on_shutdown")`.

## Connecting: The Channel Owns the Connection

On the client, `tonic::transport::Channel` is the thing we cannot use. It calls
`tokio::spawn` and hard-codes tokio's executor and timer. Its job is still
needed: connect lazily, reconnect after a loss, multiplex everything over one
connection. `ReconnectingChannel` does that job over the providers:

```rust
type GrpcChannel = ReconnectingChannel<SimProviders, tonic::body::Body>;

let channel: GrpcChannel = ReconnectingChannel::new(
    ctx.providers(),
    server_ip.clone(),
    ChannelConfig {
        connection_timeout: CONNECT_TIMEOUT,
        keep_alive: Some(keep_alive()),
        vectored_writes: true,
        ..ChannelConfig::default()
    },
);

let echo_client = EchoClient::with_origin(channel.clone(), origin.clone());
```

The channel is a tower `Service`, so the generated tonic client takes it
directly. One channel serves the whole workload, cloned into as many clients as
you like. When Attrition kills the server mid-round, the next round does not
connect by hand. It finds that the channel rebuilt itself. That is reconnection
under test, not reconnection assumed.

`ChannelConfig` defaults:

| Field | Default |
|-------|---------|
| `initial_reconnect_delay` | 100 ms, doubles per consecutive failure |
| `max_reconnect_delay` | 30 s |
| `connection_timeout` | 5 s, applied to the TCP connect and to the h2 handshake |
| `max_connection_failures` | `None`, retry forever |
| `keep_alive` | `None` |
| `vectored_writes` | `false` |

When a workload ends or a peer goes away, close the shared lifecycle:

```rust
channel.close();
assert!(channel.is_closed());
```

`close` is terminal, idempotent and shared by every clone. It interrupts an
active connect or backoff, drops the live h2 driver and its keepalive work, and
makes parked readiness checks, new requests and in-flight request futures
return `ChannelError::Closed`. It does not replay RPCs. Only the caller knows
whether an RPC is safe to retry.

`ChannelError` names the other outcomes: `Connect`, `ConnectTimeout`,
`Handshake`, `Request`, `NotReady` and `ExhaustedRetries`. Through tonic they
arrive as `Code::Unknown`, because tonic's h2-aware status mapping lives behind
features the runtime-free core does not enable. The example gives transport
death its own `assert_sometimes!` so it stays distinct from an
application-level `UNAVAILABLE`.

At the tower boundary, a failed connect is **reserved** for one caller:
`poll_ready` returns `Ready(Ok(()))` and the following `call` returns the error.
Tower treats `Ready(Err(_))` as terminal for that service instance, so the
channel uses it only for explicit shutdown and an exhausted
`max_connection_failures`. tonic's own reconnect service follows the same rule.

## The Sealed Bounds

hyper's h2 builders require `Http2ClientConnExec` and `Http2ServerConnExec`.
Those traits are **sealed**. You cannot implement them. You satisfy them by
supplying an executor that already implements `rt::Executor` for hyper's
private future types. Naming them in a generic signature compiles, but nothing
proves the bound holds until a concrete type instantiates it. moonpool-hyper's
unit tests do that with `H2Server<TokioProviders>` and
`ReconnectingChannel<TokioProviders, Full<Bytes>>`. If a wrapper of your own
refuses to compile, this is usually why.

## Why Any of This Is Deterministic

Five choices keep h2 behavior reproducible from a seed.

**Keepalive runs on provider time.** With a timer configured, hyper reads the
clock only through `Timer::now()`, and `HyperTimer` answers from the time
provider. A ping interval of 3 seconds is 3 seconds of simulated time. A
connection that dies because a clogged link swallowed the PONG dies at the same
simulated instant on every replay.

**Backoff has no jitter.** Production jitter desynchronizes a fleet of clients.
Here it would make reconnect timing depend on something other than the seed.
`ChannelConfig` doubles from `initial_reconnect_delay` and saturates at
`max_reconnect_delay`.

**Response headers do not read the wall clock.** hyper adds a `Date` header
from `SystemTime` by default. That timestamp enters the connection's HPACK
state, so two identical replays can emit different frame sizes and perturb
later network choices. `H2Server` disables the automatic header. The `builder()`
escape hatch lets a production caller turn it back on.

**Reset streams expire by count, never by wall clock.** h2 keeps a locally
reset stream around so late frames can be ignored, and by default it judges
that expiry on `std::time::Instant::now()` after one real second. A slow
machine would then expire streams at a different simulated instant.
`ReconnectingChannel` sets `reset_stream_duration` to `Duration::MAX`, so the
count cap alone bounds them. hyper exposes no such knob on the server builder,
so a server that resets streams keeps a small dependence on wall time there.

**A connection has to earn its reset.** The failure count clears only once a
connection has survived `initial_reconnect_delay`, not when the handshake
completes. Without that rule, a peer that accepts and dies at once reconnects
forever at zero backoff. With it, a flapping peer faces escalating delays,
which is the correct production behavior and a bounded one to simulate.

## Seeing It Pay Off

[`metastable_grpc_retry_storm`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool/examples/metastable_grpc_retry_storm.rs)
builds the smallest gRPC system that shows a metastable failure: a 650 ms
slowdown that pushes an open-loop client into a retry storm the server never
recovers from. It uses `H2Server`, `ReconnectingChannel` and the prometheus
adapter, and prints an ASCII time series per seed:

```text
cargo run --release --example metastable_grpc_retry_storm \
    --features hyper,prometheus -- --seed 1
```

Seed 1 storms. Seed 0 recovers. Same code, same load, and the seeded network
jitter decides which stable state the system lands in. That experiment works
because every timer and every spawn in the stack runs on provider time.
