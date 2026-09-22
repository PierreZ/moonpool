# Typed RPC with moonpool-rpc

<!-- toc -->

Raw TCP gets us bytes. Most distributed systems want something one level up: "call this method on that process and tell me what happened". FoundationDB built exactly that in `fdbrpc`, and a decade of simulation testing taught it which questions an RPC layer must answer honestly. **Did the server run my request? Can I retry it? What happens to a reference after the process behind it restarts?** `moonpool-rpc` is our answer to those questions, written once against the provider traits so the same code runs on real sockets and inside the simulator.

## Endpoints Are Allocated, Not Declared

A server does not open a port per service. It runs one `RpcDriver`, and registers endpoints on it at runtime:

```rust
let (driver, rpc) = RpcDriver::listen(providers, "10.0.1.1:4500", RpcConfig::default()).await?;
// The driver owns every socket, registration and pending call. Poll it next to
// the application: in a simulation, inside the process body, so a crash drops it.
let (echo_ref, mut requests) = rpc.register::<Echo>(AccessClass::Public)?;
```

`echo_ref` is a `ServiceRef<Echo>`: plain routing data we can hand to anyone, as bytes if we like (`to_bytes` / `from_bytes`, no runtime needed to decode it). It names three things. The **resolved address** of the process. The **incarnation**, 128 random bits drawn from the `RandomProvider` when the runtime started. And the **endpoint token**, a registry slot plus a generation that advances every time the slot is reused. A method is a marker type with explicit identifiers, never derived from Rust names:

```rust
struct Echo;
impl RpcMethod for Echo {
    type Request = Probe;          // any #[derive(prost::Message)] Rust struct
    type Reply = Echoed;
    const METHOD: MethodId = MethodId::new(0x0100);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}
```

Serving is pull-style: `requests.recv().await` yields an `IncomingRequest` with the decoded body and a one-shot `ReplyHandle`. Dropping the receiver **destroys the endpoint**. Dropping a reply handle without answering tells the caller the promise was broken.

## What Admission Checks, and When

Before a single byte of the body reaches the codec, the server checks the incarnation, the token (slot and generation), the method, the schema version and the codec, in that order. A restarted process at the same `ip:port` rejects its predecessor's references with `StaleIncarnation`. A reused slot never answers an old token. A caller compiled against schema v2 gets `SchemaMismatch` instead of garbage. Local calls go through the same admission path, same bytes, same limits, so a caller cannot tell a local endpoint from a remote one except by latency.

## Every Failure Says What It Proves

`try_get_reply` is **one attempt**. It executes zero or one times, and nothing is ever retransmitted behind our back. When it fails, the `RpcError` carries two separate things: a reason, and what that reason proves about execution.

| Execution | Meaning | Examples |
|---|---|---|
| `NotAdmitted` | No handler ever saw this attempt | endpoint not found, stale incarnation, overloaded, connect failed, request still queued when the deadline hit |
| `MaybeExecuted` | It may have run | disconnect after the frame left, broken promise, timeout after transmission |
| `Executed` | It ran, the reply was unusable | reply too large, reply that does not decode |

The runtime earns those claims. Requests wait for the peer's handshake before they are written, so a session refused at the handshake fails its calls as `NotAdmitted`. A caller that gives up withdraws its request if it is still queued, so its timeout can honestly say `NotAdmitted`. The campaign below found the case where it could not: an abandoned call whose frame was still queued behind the handshake went out anyway, and the server ran a request the caller had been told was never admitted.

## The Wire

Every frame is `u32 length | u64 XXH3-64 checksum | payload`, all little-endian. Like FoundationDB's `scanPackets`, the length is bounded before the payload is buffered, and the checksum, which also covers the length, is verified before anything is parsed. A mismatch closes the session: we never try to resynchronise a byte stream whose framing we no longer trust. The payload is a hand-written, versioned envelope carrying the routing fields and a codec id, so the transport can route, bound and reject a request without understanding its body. Bodies are protobuf through prost by default, behind the `Wire` trait. Sessions pass through a `Connector`/`Acceptor` upgrade seam (plaintext today) and open with a `Hello` carrying the supported protocol version range and each side's frame limit. That last field matters more than it looks: without it, a reply one byte over the caller's limit would make the caller tear down a session shared by every other call to that process. With it, the oversized reply fails only its own call, as `ReplyTooLarge`, and everything else keeps flowing.

## Testing It the Way It Fails

The `sim-rpc-foundations` campaign in `moonpool-rpc-sim` runs a server group, a relay group that calls the server itself, and a surviving client workload, under swarm network chaos with `Chaos::BuggifyKnobs` (which spikes the bit-flip rate on some seeds), a scripted crash right after a handler receives a request, raw malformed sessions, forged references and destroyed endpoints. The oracle is deliberately **outside the transport**: handlers write a receipt ledger keyed by workload-generated request ids before replying, and at the end the workload judges its own outcomes against it. At most one receipt per id, exactly one for a reply, none for anything reported `NotAdmitted`. It never asks the RPC runtime what it thinks happened.

```bash
cargo xtask sim run rpc-foundations
```

Every seed runs twice under the determinism canary, and the nextest suite pins the bounded scenarios: a fixed seed budget that must hit every required path, a semantic replay check, and a bit-flip-only run proving corrupted frames never reach a handler.
