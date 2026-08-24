# The hyper Stack: gRPC and HTTP

<!-- toc -->

The axum chapter got a web service into simulation by handing hyper a simulated
stream. That works because HTTP/1 asks almost nothing of the runtime: parse
bytes, write bytes, done. HTTP/2 is a different animal. It multiplexes streams
over one connection, it pings the peer to notice death, and it runs per-request
work concurrently. To do any of that, hyper needs to **spawn tasks** and
**read a clock**.

hyper is runtime-agnostic about both. It defines `rt::Executor` and `rt::Timer`
traits and asks you to supply them. In production everyone supplies
`hyper-util`'s tokio versions, which is why the ecosystem reads as
tokio-only. Inside a simulation those are exactly the two things we cannot
allow: a tokio spawn never runs on the deterministic executor, and a tokio
sleep runs on wall-clock time that no seed controls.

`moonpool-hyper` supplies the same hooks over the provider traits.

| Type | What hyper asks for | What it uses |
|------|---------------------|--------------|
| `HyperExecutor` | `rt::Executor`, spawn internal tasks | `TaskProvider` |
| `HyperTimer` | `rt::Timer`, sleeps and `now()` | `TimeProvider` |
| `HyperIo` | `rt::Read` + `rt::Write` | any `NetworkProvider` stream |
| `TowerToHyperService` | a hyper `Service` | your tower service |
| `ReconnectingChannel` | (client plumbing) | the whole provider bundle |
| `H2Server` | (server plumbing) | the whole provider bundle |

The first four are thin adapters. The last two are the interesting ones,
because they are where the plumbing that every hyper user hand-writes gets
written once.

## Serving: One Call Per Connection

Here is the gRPC server from
[`tonic_grpc.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim-examples/src/tonic_grpc.rs),
the whole thing:

```rust
let server = H2Server::new(ctx.providers()).with_config(H2ServerConfig {
    keep_alive: Some(keep_alive()),
    vectored_writes: true,
});

loop {
    moonpool_sim::select! {
        accept = listener.accept() => {
            let (stream, addr) = accept?;

            // Wired to the shutdown token: a graceful Attrition reboot
            // signals it, and hyper then finishes the RPCs already in
            // flight before the connection ends.
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
`serve_connection_with_shutdown` wraps it, builds the IO, and hands back a
future that is `Send + 'static`, so it spawns as a normal sim task. h2
connection futures are `Send`, unlike hyper's HTTP/1 state machine, which is
why this is a spawn rather than the inline `FuturesUnordered` dance the axum
example needs.

Notice what stayed with the caller: **the accept loop**. That is deliberate.
The race between accepting a new connection and making progress on existing
ones is a real ordering question, and under the seeded `select!` different
seeds explore both answers. Hiding it inside a helper would hide a property
worth testing.

The shutdown future is the graceful drain. When Attrition reboots the process
politely, the token fires, hyper stops accepting new streams on that
connection, and the RPCs already in flight get to finish. The example asserts
that this actually happens, with `assert_sometimes!(true,
"grpc_server_drained_on_shutdown")` guarded on the token being cancelled when
the connection ends.

## Connecting: The Channel Owns the Connection

On the client, `tonic::transport::Channel` is the thing we cannot use. Its job,
though, is one we still need: connect lazily, reconnect after a death, and
multiplex everything over one connection. `ReconnectingChannel` is that job,
over the providers:

```rust
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

One channel for the whole workload, cloned into as many generated clients as we
like. The example creates it once and reuses it across every round, which is
the point: when Attrition kills the server mid-round, the next round does not
build a fresh connection by hand, it discovers that the channel rebuilt itself.
That is reconnection under test rather than reconnection assumed.

When the owner removes a peer or ends a process incarnation, shut down that
shared lifecycle explicitly:

```rust
channel.close();
assert!(channel.is_closed());
```

Shutdown is terminal, idempotent, and shared by every clone. It interrupts an
active connect or reconnect backoff immediately, drops the live h2 driver (and
therefore its keepalive work), and makes parked readiness checks, new requests,
and already-issued request futures return `ChannelError::Closed`. It does not
replay logical RPCs; deciding whether a request is safe to retry remains the
application's job.

Failures arrive as `Code::Unknown`, because tonic's h2-aware status mapping
lives behind features the runtime-free core does not enable. Under chaos that
is an expected outcome, not a bug, and the example gives it its own
sometimes-assertion so transport death stays distinguishable from an
application-level `UNAVAILABLE`.

At the tower boundary, a transient connect or handshake failure is reserved by
`poll_ready`, which returns `Ready(Ok(()))`; the following `call` returns the
reserved error. Tower treats `Ready(Err(_))` as terminal for that service
instance, so the channel uses readiness errors only for terminal shutdown or an
exhausted `max_connection_failures` limit. This is also the convention tonic's
own reconnect service follows.

## The Sealed Bounds

One wrinkle worth knowing before you write your own helper. hyper's h2 builders
require `Http2ClientConnExec` and `Http2ServerConnExec`, traits that are
**sealed**: you cannot implement them, you can only satisfy them by supplying
an executor that already implements `rt::Executor` for hyper's private future
types. Naming them in a generic signature works, but nothing proves the bound
is satisfiable until a concrete type instantiates it. moonpool-hyper's tests
do exactly that, naming `ReconnectingChannel<TokioProviders, Full<Bytes>>` and
`H2Server<TokioProviders>` so the compiler has to discharge both bounds. If you
build your own wrapper and it will not compile, that is usually this.

## Why Any of This Is Deterministic

Three deliberate choices keep h2 behavior seed-reproducible.

**Keepalive runs on provider time.** When a timer is configured, hyper reads
the clock exclusively through `Timer::now()`, and `HyperTimer` answers from the
time provider. So a ping interval of three seconds means three seconds of
simulated time, and a connection that dies because a clogged link swallowed the
PONG dies at the same simulated instant on every replay.

**Backoff has no jitter.** Production jitter exists to desynchronize a fleet of
clients. Here it would only make reconnect timing depend on something other
than the seed, so `ChannelConfig` doubles from `initial_reconnect_delay` and
saturates at `max_reconnect_delay`, full stop.

**Response headers do not read the wall clock.** Hyper normally adds a `Date`
header from `SystemTime`. That timestamp becomes part of the connection's HPACK
state, so two otherwise identical gRPC replays can emit different frame sizes
and perturb later simulated network choices. `H2Server` disables the automatic
header. A production caller using the builder escape hatch can re-enable it, or
the service can provide a date from an application-controlled clock.

**A connection has to earn its reset.** The failure count that drives the
backoff clears only once a connection has survived `initial_reconnect_delay`,
not the moment the handshake completes. Without that rule, a peer that accepts
and immediately dies would reconnect forever at zero backoff and never reach
`max_connection_failures`. With it, a flapping peer faces escalating delays,
which is both the correct production behavior and a bounded one to simulate.
