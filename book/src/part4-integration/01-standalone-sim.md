# Using moonpool-sim Standalone
<!-- toc -->

**moonpool-sim is a standalone simulation engine.** It does not ask you to
adopt an actor system or an RPC framework. Provider traits, chaos injection,
assertions and fork-based exploration work with the protocol your application
already speaks: raw TCP, HTTP/1 through hyper, axum behind hyper, tonic gRPC
over h2, or a service whose database boundary is a trait with an in-memory
fake behind it.

Most teams do not design a new networking stack. They run services behind a
load balancer, call databases, and ship features. Moonpool takes control of the
runtime boundaries (time, tasks, sockets, randomness, files) and leaves the
application code recognizable.

## The Technical Foundation

One trait bound makes this possible. It lives in `moonpool-core`:

```rust
pub trait NetworkProvider: Clone + Send + Sync + 'static {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    // bind, connect
}
```

`AsyncRead` and `AsyncWrite` here are the runtime-agnostic `futures::io`
traits. `SimTcpStream` implements them, and so does the `Compat`-wrapped
`tokio::net::TcpStream` that `TokioNetworkProvider` hands out in production.
Any library that reads and writes through trait-based I/O can therefore sit on
top of a simulated stream. hyper 1.x defines its own `rt::Read` and
`rt::Write` traits, and `moonpool_hyper::HyperIo` implements them over the
futures-io shape. One thin adapter, and hyper, axum, tonic and tower run
unchanged.

This is a design choice, not luck. The provider traits match the shape the
tokio ecosystem already uses so that existing libraries need no fork.

## Proof: Real HTTP Over Simulated TCP

The integration test in
[`crates/moonpool-sim/tests/hyper_http.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim/tests/hyper_http.rs)
runs an unmodified hyper HTTP/1.1 server over simulated TCP with network
chaos. The server process is short:

```rust
#[async_trait]
impl Process for HyperServer {
    fn name(&self) -> &'static str { "server" }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        // moonpool's select!, not tokio's. `biased;` because shutdown is a
        // guard on the accept, not a peer racing it.
        let (stream, _addr) = moonpool_sim::select! {
            biased;
            result = listener.accept() => result?,
            () = ctx.shutdown().cancelled() => return Ok(()),
        };

        // SimTcpStream is futures-io; HyperIo presents it as hyper IO.
        let io = HyperIo::new(stream);
        hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service_fn(handle_request))
            .await?;
        Ok(())
    }
}
```

From hyper's point of view nothing changed. The HTTP parser, chunked encoding,
keep-alive logic and content-length checks all run for real, over a network
that delays, drops and corrupts bytes.

The client side mirrors it: `ctx.network().connect(addr)`, wrap the stream in
`HyperIo`, hand it to `hyper::client::conn::http1::handshake`. The result is
real request and response cycles over a network that tries to break them.

HTTP/2 asks more of the runtime. It spawns tasks and reads a clock.
[The hyper Stack](./08-hyper-stack.md) shows what `moonpool-hyper` supplies
for that, and what it gives you for gRPC.

## Spawning Inside a Simulation

The simulation is **single-threaded by construction**. Every iteration runs on
the [deterministic executor](../part2-foundations/11-executor.md), so every
spawned task runs on the one OS thread that steps the event queue. Determinism
depends on it.

The types that cross trait boundaries are `Send + 'static`: the provider
traits, `SimTcpStream`, `Process`, `Workload` and the `SimContext` you receive.
Application state can use `Arc<RwLock<…>>`, `DashMap` or `Arc<AtomicBool>`
without ceremony. There are two ways to spawn:

```rust
// Through the provider seam. The same call lands on tokio in production
// and on the deterministic executor in simulation.
ctx.task()
    .spawn_task("serve-one", async move { serve_one(stream).await })
    .detach();

// Directly on the executor, for test-only code.
let handle = moonpool_sim::executor::spawn("serve-one", async move {
    serve_one(stream).await
});
```

Prefer `ctx.task().spawn_task(name, future)`. The name shows up when you debug
a seed, and the code stays runnable on `TokioProviders` later.

**Do not use `tokio::spawn`, `tokio::task::spawn_local` or a `LocalSet`.**
There is no tokio runtime inside a simulation. A bare `tokio::spawn` panics,
and tokio's `spawn_local` has no `LocalSet` to attach to. When a task holds
`Rc` or `RefCell` state, spawn it with `ctx.task().spawn_local(name, future)`
or `moonpool_sim::executor::spawn_local`.

One case still bites. Some third-party connection futures are `!Send` for
reasons unrelated to moonpool. hyper's HTTP/1 `Connection` is the canonical
example: its state machine holds internal `Rc<…>` cells. You cannot spawn it
on any Send-bounded executor, moonpool's included. The fix is to **drive it
inline** next to your other work:

```rust
let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

// hyper's HTTP/1 connection future is !Send by its own design.
// Race it against the requests instead of spawning it.
moonpool_sim::select! {
    biased;
    result = send_requests(&mut sender) => result?,
    () = conn => {}
}
```

That is a hyper HTTP/1 decision, not a moonpool constraint. h2 connection
futures, axum handlers, tonic services and tower middleware are all `Send`.
Spawn them freely.

## What This Means For You

If your application talks through trait-based I/O (hyper, axum, tonic and most
of the ecosystem do), you can simulate it. The procedure is:

1. Define a `Process` that binds a listener and serves connections.
2. Define a `Workload` that connects, sends requests and checks the answers.
3. Replace each external dependency with a trait and a fake
   ([Where to Draw the Line](./02-mock-boundaries.md)).
4. Wire them together with `SimulationBuilder`.
5. Run many seeds with chaos enabled.

[Wiring a Web Service](./03-wiring-a-web-service.md) walks through all five
steps with axum. No new programming model is required. Your existing handlers
run over a network that tries to break them.
