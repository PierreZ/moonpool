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

`echo_ref` is a `ServiceRef<Echo>`: plain routing data we can hand to anyone. It is itself a protobuf message, so it travels as a field of our own messages or as bytes (`to_bytes` / `from_bytes`), and no runtime is needed to decode it. It names three things. The **resolved address** of the process. The **incarnation**, 128 random bits drawn from the `RandomProvider` when the runtime started. And the **endpoint token**, a registry slot plus a generation that advances every time the slot is reused. A method is a marker type with explicit identifiers, never derived from Rust names:

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

## Reply Streams

Some answers do not fit one reply: a range scan, a changefeed, a log tail. FoundationDB answers those with `getReplyStream`: one request, then an ordered stream of replies, paced so a slow reader cannot make the server buffer without bound. We have the same shape. A method opts in with `const STREAMING: bool = true`, the caller opens the stream, and the handler turns its reply handle into a producer:

```rust
// Caller: one registration attempt, then a futures::Stream of items.
let mut rows = scan_ref.bind(&rpc).get_reply_stream(&ScanRange { from, to })?;
while let Some(row) = rows.next().await {
    let row = row?; // items in order, then Ok(None), or exactly one Err
}

// Server: the handler's ReplyHandle becomes a StreamProducer.
let IncomingRequest { request, reply } = requests.recv().await.expect("open");
let producer = reply.into_stream().expect("a streaming method");
for row in store.range(request.from, request.to) {
    producer.send(&row).await?; // waits while the caller's window is full
}
let _ = producer.finish(); // or producer.fail(code), or drop it: a broken promise
```

The pacing is **credit counted in consumed bytes**. The caller announces a window when it opens the stream (`StreamPolicy::window_bytes`, 1 MiB by default, or `get_reply_stream_with_window`). Every item counts its whole frame, a unit both sides compute from the same bytes (`protocol::stream_item_frame_len`). `send` reserves an item's size atomically and in arrival order, so several tasks sending through one producer can never oversubscribe the window, and it waits while the window is used up. The caller acknowledges an item when its *application takes it*: on arrival if the application was already waiting for it, when it is popped from the queue otherwise. Bytes read off the socket earn nothing. That is the whole point, and the simulation proves it the blunt way: a variant that acknowledged on read broke the window bound, measured against the consumers' own ledger, more than 100,000 times in 40 seeds.

A few rules keep the stream honest:

- **Oversized items are refused, not parked.** An item larger than the window (or the frame limit) fails its `send` with `SendError::TooLarge` at once, and the stream stays open. FoundationDB lets one item overshoot once anything is free; we prefer a defined refusal to a window that means "roughly".
- **Order is checked, not assumed.** Items carry sequence numbers and the end carries the item count. A gap, a repeat, more unconsumed bytes than the window, or an end that disagrees ends the stream with `StreamProtocol` instead of skipping. The producer checks acknowledgements the same way: a repeat is ignored; one that regresses, exceeds what was sent or lands between two items ends the stream.
- **One terminal outcome, after every item.** A normal end, the producer's `fail(code)` (`StreamFailed`), a broken promise and a disconnect all arrive *after* every item that reached the caller, and the end needs no credit, so an error gets through even when the window is exhausted. Once an item arrived, every later error says `Executed`.
- **Abandoning a stream stops the producer, and rolls nothing back.** Dropping a `ReplyStream` withdraws a request that has not left yet; otherwise it sends a cancel, the producer's next `send` fails with `SendError::Cancelled` and its unwritten items are dropped. Effects the handler already had stay.
- **No resumption.** One request opens a stream, once. A broken connection ends it on both sides; carrying on is a new stream, which is the application's decision (a new incarnation's reference, a resumption point in its own request).

## Admission, Budgets and Push-Back

Every queue the runtime owns has a budget, per endpoint, per connection and per runtime (`ResourceLimits`, `StreamPolicy`): queued requests and their bytes, requests admitted but not yet answered, reliable retention, streams and the sum of their windows, control frames. A budget is enforced where work would *enter*, and a refusal there is `Overloaded` with `NotAdmitted`: nothing was queued, sent or handed to a handler. Nothing already admitted is ever dropped to make room.

The interesting one is replies. In the first package replies shared the control queue, so a caller pipelining requests to a server whose writer was stuck could fill that queue and get the whole session closed, failing unrelated calls. Now each admitted request reserves the room of its reply, and beyond the in-flight budget new requests are refused before admission: a slow writer pushes back instead of disconnecting. A real-TCP test sends 200 requests with 256 KiB replies from a peer that never reads; the server admits what fits, refuses the rest, and keeps the session.

Progress under saturation comes from ordering, not from a scheduler. The writer sends control frames first (handshake, pings, pongs, rejections), then the coalesced acknowledgements and cancels of the streams it consumes (only the latest cumulative acknowledgement per stream is ever queued), then one data frame per source in turn: the request queue, the reply queue and each produced stream. A busy stream delays another stream's next item by one frame, not by its backlog; a control frame waits for at most the one data frame being written, bounded by the frame limit. The socket reader never waits on an application queue: every frame is queued, answered or refused on the spot, and reader and writer both yield after a bounded batch (`max_frames_per_batch`). There is no priority API: FoundationDB's task priorities become these internal orderings.

The defaults were measured with the `stream_saturation` example on localhost (a debug build, four cores; the shape matters, not the absolute numbers):

| Window | MiB/s, one stream of 4 KiB items |
|---|---|
| 16 KiB | 39 |
| 64 KiB | 68 |
| 256 KiB | 53 |
| 1 MiB | 87 |
| 4 MiB | 86 |

| `max_frames_per_batch` | unary p50 | p99 | beside eight saturating streams |
|---|---|---|---|
| 1 | 157 ms | 3.09 s | yields so often the session crawls |
| 16 | 38 ms | 52 ms | |
| 64 | 39 ms | 54 ms | the default |
| 256 | 62 ms | 122 ms | |

So the window defaults to 1 MiB (FoundationDB uses 2 MB) and the batch to 64. A burst of 4096 slow calls against the defaults admits 1024 (the request queue) and refuses the other 3072 as `Overloaded`, `NotAdmitted`, with the session still open.

## Peers, Reconnects and Liveness

Every remote address has one **selected connection**, dialed on demand. When it ends, the next dial waits out a jittered backoff that grows while connections keep failing and resets only after one stayed up for a while, exactly FoundationDB's `connectionKeeper`. While a connection lives, the side using it for calls pings it; if nothing at all arrives within the ping timeout the connection is declared dead. That is how a black-holed peer, which TCP alone would never report, turns into an ordinary disconnect. A connection with no calls, no queued frames and no replies owed for the idle timeout is closed quietly; the next call dials again.

The side that only *serves* a connection does not ping it: its dialer does. But a dialer can vanish behind a half-open session, and if replies or streams are still owed to it, "idle" never comes. So, like FoundationDB's `connectionMonitor` for incoming connections, a served session that has heard nothing for the inbound idle timeout is probed with a ping of its own and failed if nothing answers. The streams campaign found exactly this: a consumer that gave up during a partition left its producer blocked on credit forever.

Two listening runtimes that call each other share one connection. Each `Hello` names the sender's listen address (when it shares sessions), so an accepted session can become the receiver's own connection to its dialer. When both dial at the same instant, the runtime with the smaller canonical address adopts the other's dial and closes its own, which is FoundationDB's rule seen from the side that gives way. Unlike FoundationDB, the larger side never closes the other's dial: a peer that will not or cannot adopt simply keeps one connection per direction, instead of watching its dials being refused forever. FoundationDB's `ALWAYS_ACCEPT_DELAY` is here too, as `always_accept_after`: if our own dials to a peer have not established for that long, we use the peer's session to us, so a peer we cannot dial (a firewall that only lets it dial out) is still reachable. Frames that were queued but never written move to the adopted connection; anything already written rides the old one down and fails (or, for a reliable call, is sent again).

With plaintext sessions that listen address is only a claim. `InboundSharing::SameIp`, the default, adopts a session only if its socket really comes from the claimed IP, which stops a process on another host from posing as a peer but not one on the same host, and refuses peers behind NAT (they keep one connection per direction). `Trusted` takes the claim at face value, as FoundationDB does; `Disabled` never shares. Authenticated peer identity is the security package's job.

All of this timing is one plain struct, `PeerPolicy`, inside `RpcConfig`. There is no policy trait: it is data, and the simulation campaign pushes every field to an extreme on some seeds.

## The Failure Monitor

`RpcHandle::failure_monitor()` answers "is this worth calling?" and lets us wait for the answer to change. It keeps three facts apart. An **address** is failed only after connections to it kept failing for `failure_detection_delay`, and available again as soon as a session establishes. A **disconnect** is counted the moment a connection ends, even while the address still counts as available, so a caller bound to that connection never waits for the failure delay. A **permanent endpoint failure** (a destroyed endpoint, a stale incarnation) is learned from the server's own rejection and remembered in a bounded cache, so the next call to that reference fails at once, without a round trip, while calls to other endpoints at the same healthy address carry on.

Every `on_*` wait (`on_disconnect`, `on_failed`, `on_available`, `on_failed_for`) captures what it compares against when it is created and registers before it checks, on a versioned latch. A change that lands between the check and the wait still wakes it; spurious wakeups are allowed and each wait re-checks. `on_failed_for(endpoint, sustained, slope)` is FoundationDB's `onFailedFor`: it resolves once the endpoint failed for good, or its address stayed failed for `sustained` plus `slope` times the time already waited, and while it waits the runtime keeps probing the failing address so the verdict can change.

## Bootstrap Names and Well-Known Endpoints

Dynamic references carry resolved addresses and die with their incarnation. Bootstrapping needs the opposite: a fixed service at a name that survives restarts. `register_well_known::<M>(WellKnownId::new(1), ..)` registers one, and its admission ignores the incarnation, so a `WellKnownRef` keeps working across server restarts. A `BootstrapClient` resolves the `host:port` in a well-known reference through a `moonpool_core::Resolver`, caches the answer for a TTL, and drops it when a call through it fails to connect or loses its connection. `retry_get_reply` retries under an explicit `RetryPolicy`: never after a contract error, always after a failure proven `NotAdmitted` (the name did not resolve, nothing listened, the endpoint is not registered yet), and after an ambiguous one only if the policy says so, because only the application knows whether running it twice is safe. Lookup failure, connection failure and endpoint failure stay three distinct errors.

## Interfaces as Data

A service with several methods is one **endpoint group**: one registry slot, one incarnation, several methods told apart by their explicit method ids. The interface gets an explicit id too:

```rust
struct Kv;
impl RpcInterface for Kv {
    const INTERFACE: InterfaceId = InterfaceId::new(0x6b76_0001);
    const VERSION: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "kv";
}
impl InterfaceMethod<Kv> for Get {}
impl InterfaceMethod<Kv> for Put {}

let group = rpc.register_group::<Kv>(AccessClass::Public)?;
let mut gets = group.serve::<Get>()?;   // a RequestStream<Get>, as before
let mut puts = group.serve::<Put>()?;
let kv_ref: InterfaceRef<Kv> = group.interface_ref();
```

FoundationDB serializes one endpoint of an interface and derives the others by adding an offset (`getAdjustedEndpoint`). We keep the idea and drop the arithmetic: `kv_ref.method::<Put>()` gives a `ServiceRef<Put>` for the same address, incarnation and token, differing only in `Put`'s method id. Reordering, adding or removing methods in the source cannot retarget a reference someone stored last week, and a method the group does not serve is refused with `MethodNotFound` before any handler runs. That refusal is not final: the group may serve the method later, so it proves only that this attempt never ran, and `RetryPolicy` retries it only when told to (`retry_method_not_found`). The interface is checked too, by the server: a reference adjusted from an interface carries that interface's id and version, the request envelope carries them, and a group refuses a reference to any other interface with `InterfaceMismatch`, before decoding, even when the method id, schema and codecs happen to match. Dropping the `ServiceGroup` destroys the whole group: new requests get `EndpointNotFound`, requests still queued complete as broken promises, and a request already received finishes under its own `ReplyHandle`, which still routes to the session it came from.

Both reference types are protobuf messages with fixed tags, encoded exactly as prost encodes them (defaults left out, unknown fields and groups skipped), pinned by golden fixtures and cross-checked against a prost-derived mirror. That makes them ordinary data: a field of a request, a reply, a file on disk.

```rust
#[derive(Clone, PartialEq, prost::Message)]
struct Recruited {
    #[prost(uint64, tag = "1")]
    configuration: u64,
    #[prost(message, repeated, tag = "2")]
    members: Vec<InterfaceRef<Kv>>,
}
```

Decoding such a field cannot reject it (protobuf merges field by field), so a reference is kept exactly as decoded and checked before use: binding an `InterfaceRef` or sending through a `ServiceRef` that names another method, schema, codec or interface fails with `InvalidReference` and nothing leaves. Re-encoding never repairs it. `from_bytes` is the strict decode.

### Restarts are learned, never refreshed

A reference names one incarnation. When a process restarts at the same `ip:port`, every reference to its previous incarnation is refused with `StaleIncarnation`, remembered by the failure monitor so the next call fails without a round trip, and **never dispatched to the new incarnation**, not even by a reliable call. The new boot publishes new references; callers learn them through the application: a well-known directory, a registration, a recruitment reply. That is FoundationDB's model too (a worker re-registers with the cluster controller; nobody patches old interfaces), and it is deliberate: a fresh endpoint says nothing about whether the durable state behind it is the one we were talking to. Participant identity, configuration identity and fencing are the application's.

A callback works the same way. A `ReplyHandle` is bound to the session its request arrived on and has no byte form, so it cannot be forwarded. A callback a third party may call is an ordinary registered endpoint whose `ServiceRef` travels in the request; once dropped, it is refused like any destroyed endpoint.

## Generated Interfaces

With the optional `derive` feature, one trait generates all of the above:

```rust
#[moonpool_rpc::service(id = 0x6b76_0001, version = 1)]
pub trait Kv {
    #[method(id = 1, schema = 1)]
    async fn get(&self, key: Key) -> Value;
    #[method(id = 2, schema = 1)]
    async fn put(&self, entry: Entry);
}
```

It generates `KvInterface`, one method marker per method (`KvGet`, `KvPut`), `KvRef` (an `InterfaceRef<KvInterface>`), `KvClient<P>` whose `get()` is an ordinary `ServiceClient` with every delivery mode, `KvRequest` (one variant per method, holding the `IncomingRequest` and its `ReplyHandle`) and `KvServer`, the registered group with a fair multiplexed `next()` over its request streams, `dispatch` and `serve`. Ids stay explicit; a duplicate is a compile error. The macro owns no protocol state: a server that wants to reply later, never reply or look at the caller matches on `KvRequest` and uses the reply handle itself, and a hand-written interface with the same ids is byte-for-byte interchangeable with the generated one.

## Balancing Across Alternatives

A read that three replicas can serve should go to the one that answers fastest, skip the one that just crashed, and maybe try a second one when the first is slow. FoundationDB's `loadBalance` does all of that, and it is one of the most useful pieces of `fdbrpc`. It is also where the easiest RPC mistake hides: **sending the same request twice is only safe if the application says so.** So `moonpool_rpc::balance` keeps FoundationDB's mechanics and makes the permissions explicit.

The input is an `AlternativeSet<M>`: typed `ServiceRef`s to specific incarnations, each with a generic `Locality` (a machine id and a datacenter id, both opaque strings), and a `SetVersion` the application chooses. The balancer never refreshes a reference. When a server restarts, the application learns its new reference the way it always does and installs a strictly newer set with `replace`. A call already running keeps the set it started with, and when every alternative turns out to be permanently gone, the call fails with `StaleAlternatives` and the client's status says so.

```rust
let model = QueueModel::new(ModelConfig::default())?;
let balanced = BalancedClient::new(&rpc, set, model.clone(), BalanceConfig {
    locality: Locality::new("m1", "dc1"),
    ..BalanceConfig::default()
})?;
// Late losers update the model after their call returned: drive them.
spawn(model.collect_lagging());

// Idempotent read: retry after an ambiguous attempt, hedge a slow one.
let policy = BalancePolicy {
    retry: Retry::AfterAmbiguous,
    duplicates: Duplicates::hedged(),
    ..BalancePolicy::default()
};
let read = balanced.call(Get { key }, &policy).await?;
```

Two permissions, independent and both off by default. `Retry::AfterAmbiguous` allows another attempt after one that may have executed. `Duplicates::Permitted` allows concurrent copies: hedges, and comparison copies requested by the hooks. Failing over after an attempt that **provably** never reached a handler (a refused connection, a destroyed endpoint, a stale incarnation, an overload refusal) needs no permission, because it cannot run the request twice. In FoundationDB, `AtMostOnce` stops the retry but still sends the budgeted second request. Here the hedge timing lives inside the duplicate permission, so no retry setting can ever grant a copy. The `balanced_mutation` example shows what each permission does to a deposit whose reply is lost: at most once reports `MaybeExecuted` and stops, a retry applies a blind deposit twice, and a deposit keyed by id applies once.

Selection follows FoundationDB's queue model. The `QueueModel` keeps, per endpoint **incarnation**, the weighted outstanding work (each attempt adds the server's current penalty), the last clean latency measured on provider time, the penalty the server reports, and a temporary exclusion with a growing, jittered backoff when the application's classifier says a reply is "temporarily behind" or "overloaded" (an overload refusal from the transport counts the same way, and none of these is a clean latency sample). The default `QueueSelector` prefers the nearest alternatives by outstanding work and spills to farther ones once more than one nearby alternative is bad. Exclusions and failed addresses rank last but are never hidden for good: when everything looks unreachable, the call waits a bounded time and then probes each alternative once anyway, even one it already tried, because a failed address only changes when something dials it.

Every started attempt holds one `Reservation` in the model and gives it back exactly once. `release` consumes it and dropping it is the unclean release, so there is no path that leaks one or releases one twice: not a winner, not an error, not a caller that gives up, not a hedge that lost the race and answered a second later. Attempts still in flight when the winner answers become **late losers**, collected by `collect_lagging` so their latency still updates the model, bounded in number and, from the moment their call let go of them, in provider time. Hedges draw from a shared budget that grows whenever an attempt answers before any copy of it was sent, and runs out, as FoundationDB's `secondBudget` does.

Hooks let the application teach the balancer its own vocabulary without teaching it storage semantics: `classify` turns a reply into accept, temporarily behind or overloaded, `wants_comparison` asks for a second copy whose reply `compare` checks against the winner's, and `observe` sees every attempt start and end. A comparison copy still needs the duplicate permission and a budget unit, a failed comparison fails the call as executed, and when every primary attempt fails or is declined an accepted comparison reply wins instead of being thrown away. A failed call's `BalanceError` keeps every attempt and summarizes them honestly: executed only if something with an effect ran, otherwise ambiguous if anything may have run, and a reply the classifier declined counts as no effect.

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

The `sim-rpc-interfaces` campaign restarts three participants at their own address throughout the chaos window: held-down crashes, in-place reboots and graceful shutdowns, sometimes with registrations delayed after the boot. Each boot serves a generated role interface through its directory and recruits a fresh instance per configuration on request. The surviving client learns interfaces only from those directories and recruiters, stores every publication as bytes, calls current and ended boots alike, and forwards recruited members to a third participant, which calls them through its own runtime. The oracles are three ledgers kept outside the transport: each participant's boot count, every publication with the boot that made it, every execution with the boot and instance that ran it. A handler checks, as it runs, that the reference named its own boot and instance and that its boot is still the current one; the client checks that a stale refusal always names a boot that really ended. It also calls groups through a reference to an impostor interface whose method shares the real one's id, schema and codecs; the server must refuse it and the execution ledger must show it never ran. With the incarnation check removed from admission, most seeds go red. The campaign also caught a simulator bug: an in-place restart could poll the new boot before the executor had dropped the old one, so for a moment two boots of one process were alive at the same address.

```bash
cargo xtask sim run rpc-interfaces
```

The `sim-rpc-balance` campaign gives three servers a character per boot (fast, or slow with a penalty, faithful or divergent) and each job a fate (answered, declined as temporarily behind, promise broken, reply held past every deadline). Servers destroy and republish their endpoint now and then, and a fault script takes every alternative away, by partitioning the client from all of them or by crashing all of them, then brings them back. The surviving client balances under every combination of permissions, cancels calls during the first attempt or during the hedge, and replaces its set only from the servers' own publications. Two ledgers judge it, neither of them the queue model: handlers record every run of every job, and the observation hook records every attempt. No job may run more often than its permissions allow, a reply must come from a server that ran the job, `NotAdmitted` must mean no handler ran, and at the end every attempt must have started, ended, reserved and released exactly once. Ignoring the retry permission, or skipping the release on drop, turns most seeds red. The campaign's first run found a real lockout: a client whose alternatives all looked failed kept failing every call after the servers had recovered, because nothing ever dialed them again to find out.

```bash
cargo xtask sim run rpc-balance
```

The `sim-rpc-streams` campaign streams under all of that. A producer process serves every kind of stream a request can ask for (a count, an item size, pauses, an ending: finish, fail with a code, drop, or send until closed) and writes a producer ledger: its boot, every item it queued with its accounted size, how it ended. The surviving consumer opens streams and reads them eagerly, slowly or not at all; abandons them before and after the first item; times out on them; fills several windows while a unary call, a new stream and a cancel must still get through; bursts calls and streams against squeezed budgets; and asks for producer crashes and graceful shutdowns mid-stream. It writes its own ledger of every item its application took. Every consumed item must be the produced item at the same position; a normal end or a failure must follow every item its producer sent; a refused stream or call never reached a handler; an abandoned stream's producer must stop; a producer never runs ahead of consumption beyond its window. At the end, after the faults stopped, a fresh stream and a unary call must succeed. Acknowledging on read, forgetting the cancel, or letting the end overtake the items each turned the bounded campaign red (the first and the last on every one of its 40 seeds). The campaign also caught a real leak: a consumer that gave up during a partition left the producer's side of the session half open, owed a stream and therefore never idle, and the producer waited on credit forever. Served sessions are now probed when they fall silent, as described above.

```bash
cargo xtask sim run rpc-streams
```
