# moonpool-hyper

hyper 1.x integration for the moonpool provider traits. A tonic gRPC
service, an axum router or plain hyper runs unchanged on `TokioProviders` in
production and on the simulation providers under test.

## Why

hyper is runtime-agnostic, but every ready-made adapter (`hyper-util`'s
`TokioExecutor`, `TokioTimer`, `TokioIo`) hard-codes tokio. To run a hyper
stack inside moonpool's deterministic simulation, hyper needs the same hooks
over `TaskProvider`, `TimeProvider` and the streams a `NetworkProvider`
returns. This crate supplies them. It brings no providers and no tokio
runtime of its own.

## Quick start

```rust
use moonpool_core::{Detach, NetworkProvider, Providers, TaskProvider, TcpListenerTrait};
use moonpool_hyper::{ChannelConfig, H2Server, ReconnectingChannel};

// Server: one provider task per accepted connection.
async fn serve<P: Providers>(providers: P, addr: &str) -> std::io::Result<()> {
    let listener = providers.network().bind(addr).await?;
    let server = H2Server::new(&providers);
    loop {
        let (stream, _peer) = listener.accept().await?;
        let connection = server.serve_connection(stream, my_tower_service.clone());
        providers.task().spawn_task("h2-connection", async move {
            let _ = connection.await;
        }).detach();
    }
}

// Client: a cloneable h2 channel that reconnects; tonic takes it as transport.
let channel: ReconnectingChannel<P, tonic::body::Body> =
    ReconnectingChannel::new(&providers, "10.0.1.1:4500", ChannelConfig::default());
```

## Features

| Type | Role |
|---|---|
| `HyperExecutor<T>` | `hyper::rt::Executor` over a `TaskProvider` |
| `HyperTimer<T>` | `hyper::rt::Timer` over a `TimeProvider` |
| `HyperIo<S>` | `hyper::rt::Read` + `hyper::rt::Write` over a futures-io stream |
| `TowerToHyperService<S>` | Tower service to hyper service adapter |
| `KeepAlive` | h2 PING keepalive settings shared by client and server |
| `H2Server<P>` / `H2ServerConfig` | Serve one accepted connection with a tower service; graceful drain on shutdown, bounded by `drain_timeout` (30 s by default) |
| `ReconnectingChannel<P, B>` / `ChannelConfig` | Cloneable h2 channel: lazy connect, exponential backoff, connection timeout, optional failure budget, explicit shared `close` |
| `H2Channel<B>` | One hyper `SendRequest` as a tower service |

`HyperIo` replaces the `tokio_util::compat::Compat` plus
`hyper_util::rt::TokioIo` two-hop bridge. Every moonpool network stream
already implements the futures-io `AsyncRead` and `AsyncWrite` pair, and that
is all hyper's IO traits need.

**Determinism.** hyper's timers and tasks go through the providers, so a
connection lifecycle is reproducible from a seed. The server disables hyper's
automatic `Date` header, and the client disables h2's wall-clock expiry of
reset streams.

## Feature flags

| Feature | Default | What it adds |
|---|---|---|
| `client` | on | `ReconnectingChannel`, `H2Channel`, `ChannelConfig`, `ChannelError` |
| `server` | on | `H2Server`, `H2ServerConfig`, `ServeError`, `DEFAULT_DRAIN_TIMEOUT` |

With `default-features = false`, the runtime adapters, the IO bridge and the
service adapter still build. The facade crate exposes this crate as
`moonpool::hyper` behind its `hyper` feature.

## Examples

`crates/moonpool-sim-examples` runs a tonic gRPC service and an axum web
service on these adapters under simulation:

```sh
cargo xtask sim run tonic-grpc
cargo xtask sim run axum-web
```

## Documentation

- [API documentation](https://docs.rs/moonpool-hyper)
- [The hyper Stack: gRPC and HTTP](https://pierrez.github.io/moonpool/part4-integration/08-hyper-stack.html) in the Sim Book

## License

Apache 2.0
