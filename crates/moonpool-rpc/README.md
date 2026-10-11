# moonpool-rpc

Typed request/reply RPC between dynamically allocated endpoints, over the
moonpool provider traits. The same code runs on real TCP (`TokioProviders`)
and under deterministic simulation (`SimProviders`). The design follows
FoundationDB's `fdbrpc`. It is not wire-compatible with it.

## Quick start

```rust
use moonpool_core::TokioProviders;
use moonpool_rpc::{AccessClass, IncomingRequest, MethodId, RpcConfig, RpcDriver, RpcMethod, SchemaVersion};

struct Echo;
impl RpcMethod for Echo {
    type Request = String;
    type Reply = String;
    const METHOD: MethodId = MethodId::new(1);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

// Server: the driver owns the runtime; poll it next to the application.
let (driver, server) = RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", config).await?;
tokio::spawn(driver.run());
let (service, mut requests) = server.register::<Echo>(AccessClass::Private)?;
tokio::spawn(async move {
    while let Some(IncomingRequest { request, reply }) = requests.recv().await {
        let _ = reply.send(&request);
    }
});

// Client: bind the reference (plain protobuf data) to its own runtime.
let echo = service.bind(&client);
let reply = echo.try_get_reply(&"hello".to_string()).await?;
```

The crate docs hold the full, compiling version. With the `derive` feature,
`#[moonpool_rpc::service]` generates the methods, a dispatcher and a typed
client from one trait.

## Features

| Type | Role |
|---|---|
| `RpcDriver<P, U>` | The owned runtime: listener, connections, registry, pending calls. Poll `run()`; drop it to stop at once, or call `RpcHandle::shutdown(grace)` to drain first (`ShutdownReport`) |
| `RpcHandle<P>` | Weak, cloneable handle: `register`, `register_with`, `register_group`, `register_well_known`, `failure_monitor`, `stats`, `probe` |
| `RpcMethod` | One method: `Request` and `Reply` types, an explicit `u32` `MethodId` and a `u16` `SchemaVersion` |
| `Wire` / `CodecId` | The body codec seam. Every `prost::Message` is `Wire` with the default `prost` feature |
| `ServiceRef<M>` | A runtime-free reference and a protobuf message: address, 128-bit incarnation, 64-bit token and generation, method, schema, codecs, access class |
| `RpcInterface` / `ServiceGroup<I>` / `InterfaceRef<I>` / `InterfaceClient<P, I>` | An endpoint group (one slot, several methods), its reference and its bound client |
| `ServiceClient<P, M>` | `send` (one-way), `attempt` / `try_get_reply` / `try_get_reply_within` (at most once), `get_reply` (reliable, may duplicate), `get_reply_unless_failed_for`, `get_reply_stream` |
| `ReplyAttempt<P, M>` | One attempt as a future: keep it for a late outcome, `cancel()` it |
| `RequestStream<M>` / `IncomingRequest` / `ReplyHandle<M>` | The owned receiver, and a one-shot, session-bound responder. Dropping a reply handle is a broken promise; `never_reply()` is not |
| `ReplyStream<M>` / `StreamProducer<M>` | Reply streams: ordered items paced by consumption credit, one terminal outcome |
| `PeerPolicy` / `FailureMonitor<P>` | Reconnect backoff, ping, idle close; address availability and permanent endpoint failure |
| `WellKnownId` / `WellKnownRef<M>` / `BootstrapClient<P, R>` / `RetryPolicy` | Bootstrap endpoints that survive restarts, hostname resolution, explicit retries |
| `ResourceLimits` / `StreamPolicy` / `EndpointQueue` | Admission and buffering budgets per runtime, connection and endpoint. An endpoint can have its own admission queue |
| `RpcError` = `ErrorReason` + `Execution` | The failure reason and what it proves: `NotAdmitted`, `MaybeExecuted`, `Executed` |
| `balance::{AlternativeSet, BalancedClient, BalancePolicy, QueueModel}` | Load balancing over a versioned set of references: locality, queue model, penalties, separate retry and hedge permissions |
| `Connector` / `Acceptor` / `Plaintext` / `PeerContext` | The session upgrade seam (plaintext by default, TLS with the `tls` feature) |
| `security::{SecurityConfig, RequestVerifier, AccessPolicy, Credential, IpAllowList, UtcClock}` | Who may call what. Private endpoints fail closed by default |
| `observability::RpcMetrics` / `RpcStats` | Counters and gauges as a bounded-label `MetricsSource` |

Contracts:

- **At most once per attempt.** No hidden retry, except in `get_reply`,
  which says so.
- **Checked identity.** A request is admitted only when its incarnation,
  token, method, schema and codec match a live registration, before any
  body byte is decoded. A restarted process refuses its predecessor's
  references.
- **Bounded everything.** Work beyond a budget is refused where it would
  enter, as `Overloaded` with `NotAdmitted`. Nothing admitted is dropped to
  make room.

## Feature flags

| Feature | Default | What it adds |
|---|---|---|
| `prost` | on | Protobuf bodies from `#[derive(prost::Message)]` structs |
| `derive` | off | `#[moonpool_rpc::service]` (`moonpool-rpc-derive`) |
| `tls` | off | Server-authenticated TLS 1.3 sessions (`futures-rustls`, rustls with `ring`). Native only. No mutual TLS |
| `jwt` | off | JWT/JWKS request verification (`jsonwebtoken`, pure-Rust backend). Native only |

With `--no-default-features` the transport still builds, also for
`wasm32-unknown-unknown`; applications implement `Wire` themselves. The
security policy core is always compiled.

## Wire format

`u32 LE length | u64 LE XXH3-64(length ‖ payload) | payload`. The payload is
a hand-written, versioned envelope (kind, reply route, incarnation, token,
interface, method, schema, codec, metadata) followed by the opaque body.
Each session opens with a `Hello` that carries the supported version range.
The session runs at the highest common version (`RpcConfig::protocol_versions`).
`PING` and `PONG` carry liveness. `STREAM_ITEM`, `STREAM_END`, `STREAM_ACK`
and `STREAM_CANCEL` carry reply streams. See the `protocol`, `stream` and
`codec` module docs.

## Simulation

This crate never depends on `moonpool-sim`. It calls the inert
`moonpool-buggify` macros, which do nothing outside a simulation. Under
simulation they cut a session right behind a request, refuse a request as
`Overloaded`, defer a stream acknowledgement, or wake the failure monitor
with nothing changed. The harness, workloads and oracles live in the
non-published `moonpool-rpc-sim` crate.

## Examples

```sh
# Publish, restart and recruit interfaces on real TCP with the manual API.
cargo run -p moonpool-rpc --example recruitment
# What retry and hedge permissions do to a mutation whose reply is lost.
cargo run -p moonpool-rpc --example balanced_mutation
# Stream throughput by window, unary latency beside saturated streams.
cargo run --release -p moonpool-rpc --example stream_saturation
```

## Documentation

- [API documentation](https://docs.rs/moonpool-rpc)
- [Typed RPC with moonpool-rpc](https://pierrez.github.io/moonpool/part4-networking/02-rpc.html), [What moonpool-rpc Promises](https://pierrez.github.io/moonpool/part4-networking/03-rpc-guarantees.html) and [Admission, Security and Derive](https://pierrez.github.io/moonpool/part4-networking/04-rpc-operations.html) in the Sim Book
