# Using moonpool-sim Standalone
<!-- toc -->

moonpool, the crate, re-exports the full framework: transport, RPC, `#[service]` macros. Plenty of machinery for building distributed systems from scratch.

But **moonpool-sim is a standalone simulation engine**. Provider traits, chaos injection, assertions, fork-based exploration. All of it works without importing a single transport type. No `Peer`, no `NetTransport`, no `#[service]`. Just deterministic simulation of your existing code.

Why does this matter? Because most teams aren't building distributed systems from scratch. They're running axum services behind a load balancer, talking to Postgres and Redis, shipping features. The transport layer is irrelevant to them. The simulation engine is not.

## The Technical Foundation

The key fact that makes this possible lives in the `NetworkProvider` trait:

```rust
pub trait NetworkProvider: Clone + Send + Sync + 'static {
    type TcpStream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    // ...
}
```

`SimTcpStream` implements `tokio::io::AsyncRead + AsyncWrite + Unpin` and is `Send + Sync + 'static`. That makes it a **drop-in replacement** for `tokio::net::TcpStream` anywhere the tokio ecosystem uses trait-based I/O. And the tokio ecosystem uses trait-based I/O everywhere that matters: hyper, tonic, tower, axum (via hyper), sqlx's wire protocol, redis-rs.

This isn't an accident. We designed the provider traits to match tokio's interfaces exactly because we wanted existing libraries to work unchanged.

## Proof: Real HTTP Over Simulated TCP

The hyper integration test in `moonpool-sim/tests/hyper_http.rs` demonstrates this concretely. Unmodified hyper HTTP/1.1 running over simulated TCP with chaos injection:

```rust
struct HyperServer;

#[async_trait]
impl Process for HyperServer {
    fn name(&self) -> &str { "server" }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        let (stream, _addr) = tokio::select! {
            result = listener.accept() => result?,
            _ = ctx.shutdown().cancelled() => return Ok(()),
        };

        // SimTcpStream implements futures::io traits; .compat() bridges them
        // back to tokio's IO traits, which is what TokioIo (and hyper) expects.
        let io = TokioIo::new(stream.compat());

        hyper::server::conn::http1::Builder::new()
            .serve_connection(io, service_fn(handle_request))
            .await?;

        Ok(())
    }
}
```

`SimTcpStream` implements `futures::io::AsyncRead + AsyncWrite` (the runtime-agnostic IO traits). Hyper expects `tokio::io` traits, so we route the stream through `tokio_util::compat::Compat` via `.compat()` (from `FuturesAsyncReadCompatExt`), then hand the result to `TokioIo`. From hyper's perspective nothing has changed: HTTP parser, chunked encoding, keep-alive logic, content-length validation, all exercised for real over simulated networking.

The client side follows the same pattern. Connect via `ctx.network().connect()`, `.compat()` the stream, wrap in `TokioIo`, hand to `hyper::client::conn::http1::handshake`. Real HTTP/1.1 request-response cycles over a network that drops packets, injects latency, and kills connections.

## Spawning Inside a Simulation

The sim runtime is **single-threaded by construction**. We build it with `tokio::runtime::Builder::new_current_thread().build()`, so every spawned task runs on the one OS thread that drives simulation events. Determinism depends on it.

What changed compared to earlier moonpool versions: the types crossing trait boundaries are now `Send + 'static`. Provider traits, `SimTcpStream`, `Process`, `Workload`, the `SimContext` you get handed, all of them. That means **`tokio::spawn` is the right tool** when you want to spawn a task from inside simulated code, and customer state can use `Arc<RwLock<…>>`, `DashMap`, `Arc<AtomicBool>` without ceremony.

```rust
// Works: SimTcpStream is Send, the future is Send, tokio::spawn accepts it.
let handle = tokio::spawn(async move {
    serve_one(stream).await
});
```

If you want to keep the provider seam (so the same code runs against a real tokio runtime later), reach for `TaskProvider::spawn_task` from `ctx.task()` instead. Same semantics, an injectable interface.

**Do not use `tokio::task::spawn_local` or build a `LocalSet`**. The simulation runtime is `new_current_thread().build()`, not `build_local()`, so there is no `LocalSet` for `spawn_local` to attach to and the spawn would never poll. Whenever you reach for a `!Send` workaround, you have probably wandered off the supported path.

The one case that still bites people: some third-party connection futures (hyper's `Connection` is the canonical example) are themselves `!Send` for reasons unrelated to moonpool. They hold internal state that isn't `Send`. You cannot `tokio::spawn` *those* either, regardless of runtime. The fix is to **drive them inline** alongside your other work:

```rust
let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

// hyper's connection future is !Send by its own design.
// Drive it inline with select! instead of spawning.
let driver = async move {
    let _ = conn.await;
};

tokio::select! {
    result = send_requests(&mut sender) => result?,
    _ = driver => {}
    _ = ctx.shutdown().cancelled() => {}
}
```

That is a hyper API decision, not a moonpool constraint. Axum handlers, tonic services, tower middleware, all `Send`. Spawn them freely.

## What This Means For You

If your application uses `tokio::net::TcpStream` through trait-based I/O (which hyper, axum, tonic, and most of the ecosystem do), you can simulate it. The process is:

1. Define a `Process` that binds a listener and serves connections
2. Define a `Workload` that connects and sends requests
3. Wire them together with `SimulationBuilder`
4. Run thousands of iterations with chaos injection

No actor system required. No RPC framework. No new programming model. Just your existing HTTP handlers running over a network that tries to break them.
