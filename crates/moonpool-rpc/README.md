# moonpool-rpc

Typed request/reply RPC between dynamically allocated endpoints, over the
moonpool provider traits: the same code runs on real TCP (`TokioProviders`)
and under deterministic simulation (`SimProviders`). Inspired by
FoundationDB's `fdbrpc`; not wire-compatible with it.

## What

| Type | Role |
|------|------|
| `RpcDriver<P, U>` | the owned runtime: listener, connections, registry, pending calls; poll `run()`, drop to shut down |
| `Connector` / `Acceptor` / `Plaintext` / `PeerContext` | the session upgrade seam (plaintext by default; TLS later) |
| `RpcHandle<P>` | weak, cloneable handle: `register(AccessClass)`, `register_group`, `register_well_known`, `failure_monitor`, `stats`, `probe` |
| `RpcMethod` | one method: `Request`/`Reply` types plus explicit `u32` `MethodId` and `u16` `SchemaVersion` |
| `Wire` / `CodecId` | the body codec seam; every `prost::Message` is `Wire` (default `prost` feature) |
| `ServiceRef<M>` | plain, runtime-free reference and a protobuf message: resolved `ip:port`, 128-bit incarnation, 64-bit token + generation, method, schema, codecs, access class |
| `RpcInterface` / `InterfaceId` / `InterfaceMethod<I>` | an interface's explicit identity, and its member methods |
| `ServiceGroup<I>` / `InterfaceRef<I>` / `InterfaceClient<P, I>` | an endpoint group (one slot, several methods), its protobuf reference, adjusted to a method by explicit id, and its bound client |
| `ServiceClient<P, M>` | a reference bound to a runtime: `send` (one-way), `attempt` / `try_get_reply` / `try_get_reply_within` (one at-most-once attempt), `get_reply` (reliable, may duplicate), `get_reply_unless_failed_for` |
| `ReplyAttempt<P, M>` | one attempt as a future: keep it for a late outcome, `cancel()` for execution knowledge |
| `PeerPolicy` | plain-data reconnect backoff, ping, idle and failure-detection timing (`RpcConfig::peer`) |
| `FailureMonitor<P>` | address availability, disconnect events and permanent endpoint failures, with race-free waits |
| `WellKnownId` / `WellKnownRef<M>` / `BootstrapClient<P, R>` / `RetryPolicy` | bootstrap endpoints that survive restarts, hostname resolution through a `moonpool_core::Resolver`, explicit retries |
| `RequestStream<M>` | the owned receiver; dropping it destroys the endpoint |
| `ReplyHandle<M>` | one-shot, session-bound responder; dropping it is a broken promise, `never_reply()` is not |
| `RpcError` = `ErrorReason` + `Execution` | the failure reason, and what it proves (`NotAdmitted`, `MaybeExecuted`, `Executed`) |

## Wire format

`u32 LE length | u64 LE XXH3-64(length ‖ payload) | payload`, where the
payload is a hand-written, versioned envelope (kind, reply route, incarnation,
token, interface, method, schema, codec, reserved metadata) followed by the opaque body.
Each session opens with a `Hello` carrying the supported version range, the
runtime incarnation, reserved feature bits, the frame limit and the listen
address; `PING`/`PONG` frames carry liveness. See the `protocol`
and `codec` module docs.

## Features

- `prost` (default): protobuf bodies from `#[derive(prost::Message)]` Rust
  structs. With it off the transport still builds (including for
  `wasm32-unknown-unknown`); applications implement `Wire` themselves.
  References encode as protobuf either way.
- `derive` (off): `#[moonpool_rpc::service]` generates an interface's
  markers, a dispatcher over its request streams and a typed client from a
  trait (`moonpool-rpc-derive`), on top of the manual API.

The `recruitment` example publishes, restarts and recruits interfaces on
real TCP with the manual API:
`cargo run -p moonpool-rpc --example recruitment`.

The simulation harness, workloads and oracles live in the non-published
`moonpool-rpc-sim` crate; this crate never depends on `moonpool-sim`.
