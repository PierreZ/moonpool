# moonpool-rpc

Typed request/reply RPC between dynamically allocated endpoints, over the
moonpool provider traits: the same code runs on real TCP (`TokioProviders`)
and under deterministic simulation (`SimProviders`). Inspired by
FoundationDB's `fdbrpc`; not wire-compatible with it.

## What

| Type | Role |
|------|------|
| `RpcDriver<P>` | the owned runtime: listener, connections, registry, pending calls; poll `run()`, drop to shut down |
| `RpcHandle<P>` | weak, cloneable handle: `register`, `stats`, `probe` |
| `RpcMethod` | one method: `Request`/`Reply` types plus stable `MethodId` / `SchemaId` |
| `Wire` / `CodecId` | the body codec seam; every `prost::Message` is `Wire` (default `prost` feature) |
| `ServiceRef<M>` | plain, runtime-free reference: address, incarnation, checked token, method, schema, codecs |
| `ServiceClient<P, M>` | a reference bound to a runtime; `try_get_reply` is one at-most-once attempt |
| `RequestStream<M>` | the owned receiver; dropping it destroys the endpoint |
| `ReplyHandle<M>` | one-shot, session-bound responder; dropping it is a broken promise |
| `RpcError` / `Execution` | the failure reason, and what it proves about execution |

## Wire format

`u32 LE length | u64 LE XXH3-64(length ‖ payload) | payload`, where the
payload is a hand-written, versioned envelope (kind, reply route, incarnation,
token, method, schema, codec) followed by the opaque body. See the `protocol`
and `codec` module docs.

## Features

- `prost` (default): protobuf bodies from `#[derive(prost::Message)]` Rust
  structs. With it off the transport still builds (including for
  `wasm32-unknown-unknown`); applications implement `Wire` themselves.

The simulation harness, workloads and oracles live in the non-published
`moonpool-rpc-sim` crate; this crate never depends on `moonpool-sim`.
