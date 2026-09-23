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

## Choosing a Delivery Mode

One attempt is the default because it is the only mode that can promise "at most once". Sometimes we want more, and FoundationDB's four request styles are all here, each saying plainly what it risks:

| Call | Delivery | What a success or failure means |
|---|---|---|
| `send(&req)` | one-way, zero or one executions | `Ok` means queued; the server never answers or rejects it |
| `try_get_reply(&req)`, `attempt(&req)` | one attempt, never retransmitted | reply, or an error that says what it proves |
| `get_reply(&req)` | the request is kept in memory and sent again on every new connection | **may execute more than once**; the first outcome wins |
| `get_reply_unless_failed_for(&req, sustained, slope)` | reliable, bounded by observed failure | `PeerFailed` keeps the ambiguity of every copy sent |

Reliable delivery is about connections, not deadlines. A copy is sent again only when the connection that carried the previous one ends, so a handler that ran but whose reply died with the connection runs again. That is the price of "eventually delivered while I wait", and it is why the retention is memory only and lives exactly as long as the caller: a reply, dropping the future, a terminal endpoint failure or shutdown releases it. None of those retracts bytes already written or undoes an execution, so a failure after a copy left reports `MaybeExecuted` even when the final rejection proved nothing was admitted *that* time.

There is one place we deliberately differ from FoundationDB. When a reliable call learns that its dynamic endpoint is gone (the endpoint was destroyed, or the process restarted and is a new incarnation), FDB's `sendCanceler` stops retransmitting and then waits forever. We end the call at once with `EndpointNotFound` or `StaleIncarnation`. A reliable call never reaches a fresh incarnation behind the caller's back: learning the new reference is the application's job.

`attempt(&req)` returns a `ReplyAttempt`, one attempt as a future. Keep it after the caller has moved on (the loser of a hedged pair, say) and it still yields the late outcome; drop it and the reply route is released, so a reply arriving later is counted in `late_replies` and discarded. `cancel()` reports what is known right now: withdrawn before it left (`NotAdmitted`), or possibly executed.

On the serving side a `ReplyHandle` finishes one of three ways. `send` replies. Dropping it tells the caller the promise was broken. `never_reply()` says, on purpose, that no answer will come: nothing is sent and the caller keeps waiting under its own deadline or failure bound. A one-way request's handle reports `expects_reply() == false` and nothing it does reaches anyone.

## Peers, Reconnects and Liveness

Every remote address has one **selected connection**, dialed on demand. When it ends, the next dial waits out a jittered backoff that grows while connections keep failing and resets only after one stayed up for a while, exactly FoundationDB's `connectionKeeper`. While a connection lives, the side using it for calls pings it; if nothing at all arrives within the ping timeout the connection is declared dead. That is how a black-holed peer, which TCP alone would never report, turns into an ordinary disconnect. A connection with no calls, no queued frames and no replies owed for the idle timeout is closed quietly; the next call dials again.

Two listening runtimes that call each other share one connection. Each `Hello` names the sender's listen address, so an accepted session becomes the receiver's own connection to its dialer. When both dial at the same instant, both sides apply FoundationDB's rule: the runtime with the larger canonical address keeps the connection it dialed, the other adopts it, and each closes the loser. Frames that were queued but never written move to the winner; anything already written rides the loser down and fails (or, for a reliable call, is sent again). With plaintext sessions that listen address is self-asserted, so `PeerPolicy::share_inbound_sessions` can switch sharing off where untrusted processes can reach the listener.

All of this timing is one plain struct, `PeerPolicy`, inside `RpcConfig`. There is no policy trait: it is data, and the simulation campaign pushes every field to an extreme on some seeds.

## The Failure Monitor

`RpcHandle::failure_monitor()` answers "is this worth calling?" and lets us wait for the answer to change. It keeps three facts apart. An **address** is failed only after connections to it kept failing for `failure_detection_delay`, and available again as soon as a session establishes. A **disconnect** is counted the moment a connection ends, even while the address still counts as available, so a caller bound to that connection never waits for the failure delay. A **permanent endpoint failure** (a destroyed endpoint, a stale incarnation) is learned from the server's own rejection and remembered in a bounded cache, so the next call to that reference fails at once, without a round trip, while calls to other endpoints at the same healthy address carry on.

Every `on_*` wait (`on_disconnect`, `on_failed`, `on_available`, `on_failed_for`) captures what it compares against when it is created and registers before it checks, on a versioned latch. A change that lands between the check and the wait still wakes it; spurious wakeups are allowed and each wait re-checks. `on_failed_for(endpoint, sustained, slope)` is FoundationDB's `onFailedFor`: it resolves once the endpoint failed for good, or its address stayed failed for `sustained` plus `slope` times the time already waited, and while it waits the runtime keeps probing the failing address so the verdict can change.

## Bootstrap Names and Well-Known Endpoints

Dynamic references carry resolved addresses and die with their incarnation. Bootstrapping needs the opposite: a fixed service at a name that survives restarts. `register_well_known::<M>(WellKnownId::new(1), ..)` registers one, and its admission ignores the incarnation, so a `WellKnownRef` keeps working across server restarts. A `BootstrapClient` resolves the `host:port` in a well-known reference through a `moonpool_core::Resolver`, caches the answer for a TTL, and drops it when a call through it fails to connect or loses its connection. `retry_get_reply` retries under an explicit `RetryPolicy`: never after a contract error, always after a failure proven `NotAdmitted` (the name did not resolve, nothing listened, the endpoint is not registered yet), and after an ambiguous one only if the policy says so, because only the application knows whether running it twice is safe. Lookup failure, connection failure and endpoint failure stay three distinct errors.

## Testing It the Way It Fails

The `sim-rpc-foundations` campaign in `moonpool-rpc-sim` runs a server group, a relay group that calls the server itself, and a surviving client workload, under swarm network chaos with `Chaos::BuggifyKnobs` (which spikes the bit-flip rate on some seeds), a scripted crash right after a handler receives a request, raw malformed sessions, forged references and destroyed endpoints. The oracle is deliberately **outside the transport**: handlers write a receipt ledger keyed by workload-generated request ids before replying, and at the end the workload judges its own outcomes against it. At most one receipt per id, exactly one for a reply, none for anything reported `NotAdmitted`. It never asks the RPC runtime what it thinks happened.

```bash
cargo xtask sim run rpc-foundations
```

Every seed runs twice under the determinism canary, and the nextest suite pins the bounded scenarios: a fixed seed budget that must hit every required path, a semantic replay check, and a bit-flip-only run proving corrupted frames never reach a handler.

The `sim-rpc-delivery` campaign does the same for delivery and recovery. Its server's handler writes an execution ledger first and then does what the job asks: reply, reply late, decline to reply, break the promise, reply into a scripted cut of the caller's link, reply while being crashed, or be crashed. Two listening peers call each other at the same instants, so both dial at once. The surviving client bootstraps through a scripted name that is missing, then wrong, then right, drives every delivery mode, and watches its failure monitor. The ledger judges it: a single attempt never shows two executions, a reply shows at least one, `NotAdmitted` shows none, and reliable delivery must sometimes show two. A version of the runtime that retransmitted single attempts turned about one seed in nine red.

```bash
cargo xtask sim run rpc-delivery
```
