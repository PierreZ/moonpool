# Using moonpool-sim Standalone
<!-- toc -->

moonpool, the crate, re-exports the full framework: transport, RPC, `#[service]` macros. Plenty of machinery for building distributed systems from scratch.

But **moonpool-sim is a standalone simulation engine**. Provider traits, chaos injection, assertions, fork-based exploration. All of it works without importing a single transport type. No `Peer`, no `NetTransport`, no `#[service]`. Just deterministic simulation of your existing code.

Why does this matter? Because most teams aren't building distributed systems from scratch. They're running axum services behind a load balancer, talking to Postgres and Redis, shipping features. The transport layer is irrelevant to them. The simulation engine is not.

## The Technical Foundation

The key fact that makes this possible lives in the `NetworkProvider` trait:

```rust
pub trait NetworkProvider: Clone {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + 'static;
    // ...
}
```

`SimTcpStream` implements `tokio::io::AsyncRead + AsyncWrite + Unpin`. That makes it a **drop-in replacement** for `tokio::net::TcpStream` anywhere the tokio ecosystem uses trait-based I/O. And the tokio ecosystem uses trait-based I/O everywhere that matters: hyper, tonic, tower, axum (via hyper), sqlx's wire protocol, redis-rs.

This isn't an accident. We designed the provider traits to match tokio's interfaces exactly because we wanted existing libraries to work unchanged.

## Proof: Real HTTP Over Simulated TCP

The hyper integration test in `moonpool-sim/tests/hyper_http.rs` demonstrates this concretely. Unmodified hyper HTTP/1.1 running over simulated TCP with chaos injection:

```rust
struct HyperServer;

#[async_trait(?Send)]
impl Process for HyperServer {
    fn name(&self) -> &str { "server" }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        let (stream, _addr) = tokio::select! {
            result = listener.accept() => result?,
            _ = ctx.shutdown().cancelled() => return Ok(()),
        };

        // TokioIo bridges SimTcpStream into hyper's type system
        let io = TokioIo::new(stream);

        hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service_fn(handle_request))
            .await?;

        Ok(())
    }
}
```

The `TokioIo` adapter bridges any `AsyncRead + AsyncWrite` into hyper's internal I/O type. Because `SimTcpStream` satisfies those bounds, hyper never knows it's running over simulated networking. The HTTP parser, chunked encoding, keep-alive logic, content-length validation: all exercised for real.

The client side follows the same pattern. Connect via `ctx.network().connect()`, wrap in `TokioIo`, hand to `hyper::client::conn::http1::handshake`. Real HTTP/1.1 request-response cycles over a network that drops packets, injects latency, and kills connections.

## The Send Constraint

One constraint to understand: `SimTcpStream` is `!Send`. It lives inside the simulation's single-threaded runtime. This means you cannot use `tokio::spawn()` for futures that hold a stream reference, because `tokio::spawn` requires `Send`.

The fix is straightforward: use `tokio::task::spawn_local` instead.

```rust
// Won't compile: spawn requires Send, SimTcpStream is !Send
// tokio::spawn(async move { serve_connection(io, service).await });

// Works: spawn_local runs on the current thread
tokio::task::spawn_local(async move {
    hyper::server::conn::http1::Builder::new()
        .serve_connection(io, service)
        .await
});
```

Most web frameworks work fine under this constraint because the connection-level future holds the stream, and handlers are polled inline within that future. The handler functions themselves can be `Send` (axum requires this). The connection future that wraps them is `!Send` because it holds the stream. Both coexist because hyper polls handlers inline, never spawning them onto a separate task.

## What This Means For You

If your application uses `tokio::net::TcpStream` through trait-based I/O (which hyper, axum, tonic, and most of the ecosystem do), you can simulate it. The process is:

1. Define a `Process` that binds a listener and serves connections
2. Define a `Workload` that connects and sends requests
3. Wire them together with `SimulationBuilder`
4. Run thousands of iterations with chaos injection

No actor system required. No RPC framework. No new programming model. Just your existing HTTP handlers running over a network that tries to break them.
