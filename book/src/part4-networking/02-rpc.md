# Typed RPC with moonpool-rpc

<!-- toc -->

Raw TCP gives us bytes. Most distributed systems want one level up: "call
this method on that process and tell me what happened". FoundationDB built
that layer in `fdbrpc`, and a decade of simulation taught it which questions
an RPC layer must answer honestly. **Did the server run my request? Can I
retry it? What happens to a reference after the process behind it
restarts?** `moonpool-rpc` answers those questions. It is written once
against the provider traits, so the same runtime serves real sockets through
`TokioProviders` and the simulator through `SimProviders`. It never depends
on `moonpool-sim`.

## One Driver, Many Endpoints

A server does not open a port per service. It runs one `RpcDriver` and
registers endpoints on it at runtime:

```rust,ignore
use moonpool_rpc::{AccessClass, RpcConfig, RpcDriver};

let (driver, rpc) = RpcDriver::listen(providers, "10.0.1.1:4500", RpcConfig::default()).await?;
// The driver owns every socket, registration and pending call. Poll it next
// to the application. Inside a simulated process, a crash drops it.
let (echo_ref, mut requests) = rpc.register::<Echo>(AccessClass::Public)?;
```

`rpc` is an `RpcHandle`: a weak, cloneable handle. `echo_ref` is a
`ServiceRef<Echo>`, plain routing data we can give to anyone. It is a
protobuf message, so it travels inside our own messages or as bytes
(`to_bytes`, `from_bytes`), and decoding it needs no runtime. It names three
things. The **resolved address** of the process. The **incarnation**, 128
random bits drawn from the `RandomProvider` when the runtime started. And the
**endpoint token**, a registry slot plus a generation that advances every
time the slot is reused.

A method is a marker type with explicit identifiers. Nothing is derived from
a Rust name:

```rust,ignore
struct Echo;
impl RpcMethod for Echo {
    type Request = Probe;          // any #[derive(prost::Message)] struct
    type Reply = Echoed;
    const METHOD: MethodId = MethodId::new(0x0100);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}
```

Serving is pull-style. `requests.recv().await` yields an `IncomingRequest`
with the decoded `request` and a one-shot `reply: ReplyHandle`. Dropping the
`RequestStream` **destroys the endpoint**. A `ReplyHandle` ends one of three
ways: `send` answers, `never_reply()` says on purpose that no answer comes,
and dropping it tells the caller the promise was broken.

The caller binds a reference to its own runtime and calls:

```rust,ignore
let client = echo_ref.bind(&rpc);
let echoed = client.try_get_reply(&Probe { text: "hi".into() }).await?;
```

## Every Failure Says What It Proves

`try_get_reply` is **one attempt**. It executes zero or one times and is
never retransmitted behind our back. When it fails, the `RpcError` carries a
reason (`ErrorReason`) and what that reason proves about execution
(`Execution`):

| Execution | Meaning | Examples |
|---|---|---|
| `NotAdmitted` | No handler ever saw this attempt | endpoint not found, stale incarnation, overloaded, connect failed, still queued at the deadline |
| `MaybeExecuted` | It may have run | disconnect after the frame left, broken promise, timeout after transmission |
| `Executed` | It ran, but the reply was unusable | reply too large, reply that does not decode |

The runtime earns those claims. A request waits for the peer's handshake
before it is written, so a session refused at the handshake fails its calls
as `NotAdmitted`. A caller that gives up withdraws a request that is still
queued, so its timeout can honestly say `NotAdmitted`. The delivery campaign
found the one case where it could not: an abandoned call whose frame was
queued behind the handshake went out anyway, and the server ran a request
the caller had been told was never admitted.

## Choosing a Delivery Mode

One attempt is the default because it is the only mode that can promise "at
most once". FoundationDB's four request styles are all here, and each one
says what it risks:

| Call | Delivery | What a result means |
|---|---|---|
| `send(&req)` | one-way, zero or one executions | `Ok` means queued, nothing more |
| `try_get_reply(&req)`, `try_get_reply_within(&req, timeout)`, `attempt(&req)` | one attempt, never retransmitted | a reply, or an error that says what it proves |
| `get_reply(&req)` | kept in memory and sent again on every new connection | **may execute more than once**; the first outcome wins |
| `get_reply_unless_failed_for(&req, sustained, slope)` | reliable, bounded by observed failure | `PeerFailed` keeps the ambiguity of every copy sent |

Reliable delivery is about connections, not deadlines. A copy goes out again
only when the connection that carried the previous one ends. A handler that
ran but whose reply died with the connection runs again. The retention is
memory only and lives as long as the caller. A reply, a dropped future, a
terminal endpoint failure or shutdown releases it.

Here we differ from FoundationDB on purpose. When a reliable call learns that
its endpoint is gone (destroyed, or the process restarted as a new
incarnation), FDB stops retransmitting and waits forever. We end the call at
once with `EndpointNotFound` or `StaleIncarnation`. A reliable call never
reaches a fresh incarnation behind the caller's back. Learning the new
reference is the application's job.

`attempt(&req)` returns a `ReplyAttempt`, one attempt as a future. Keep it
after the caller moved on (the loser of a hedged pair) and it still yields
the late outcome. Drop it and a late reply is counted in `late_replies` and
discarded. `cancel()` reports what is known now: `NotAdmitted` if it never
left, or possibly executed.

## Reply Streams

Some answers do not fit one reply: a range scan, a changefeed, a log tail. A
method opts in with `const STREAMING: bool = true`. The caller opens the
stream and the handler turns its reply handle into a producer:

```rust,ignore
// Caller: one registration attempt, then a futures::Stream of items.
let mut rows = scan_ref.bind(&rpc).get_reply_stream(&ScanRange { from, to })?;
while let Some(row) = rows.next().await {
    let row = row?; // items in order, then the end, or exactly one Err
}

// Server: the ReplyHandle becomes a StreamProducer.
let IncomingRequest { request, reply } = requests.recv().await.expect("open");
let producer = reply.into_stream().expect("a streaming method");
for row in store.range(request.from, request.to) {
    producer.send(&row).await?; // waits while the caller's window is full
}
let _ = producer.finish(); // or producer.fail(code), or drop it: a broken promise
```

Pacing is **credit counted in consumed bytes**. The caller announces a window
when it opens the stream (`StreamPolicy::window_bytes`, 1 MiB by default, or
`get_reply_stream_with_window`). Every item counts its whole frame. The
caller acknowledges an item only when its application **takes it**, never
when the bytes were merely read off the socket. The streams campaign proved
the difference the blunt way: a variant that acknowledged on read broke the
window bound more than 100,000 times in 40 seeds.

Four rules keep a stream honest. An item larger than the window fails its
`send` with `SendError::TooLarge` and the stream stays open. A gap or repeat
in the sequence numbers ends the stream with `StreamProtocol`. There is one
terminal outcome, delivered after every item that arrived, and it needs no
credit. Dropping a `ReplyStream` cancels it: the producer's next `send` fails
with `SendError::Cancelled`, and nothing already done is rolled back. There
is no resumption: carrying on is a new stream the application opens.

## Peers, Liveness and the Failure Monitor

Every remote address has one **selected connection**, dialed on demand. When
it ends, the next dial waits out a jittered backoff that grows while
connections keep failing, FoundationDB's `connectionKeeper`. While a
connection lives, the dialing side pings it. Nothing within the ping timeout
means the connection is dead. That is how a black-holed peer, which TCP alone
never reports, becomes an ordinary disconnect. A served session that falls
silent while replies are still owed is probed too, because a consumer that
gave up during a partition once left its producer blocked on credit forever.

Two listening runtimes that call each other share one connection.
`InboundSharing::SameIp` (the default) adopts an accepted session only when
its socket really comes from the IP its `Hello` claims. All of this timing
is one plain struct, `PeerPolicy`, inside `RpcConfig`, so the simulation
pushes every field to an extreme on some seeds.

`RpcHandle::failure_monitor()` answers "is this worth calling?". It keeps
three facts apart. An **address** is failed only after connections to it
kept failing for `failure_detection_delay`. A **disconnect** is counted the
moment a connection ends. A **permanent endpoint failure** (destroyed
endpoint, stale incarnation) is learned from the server's own rejection and
remembered, so the next call to that reference fails without a round trip.
Every `on_*` wait (`on_disconnect`, `on_failed`, `on_available`,
`on_failed_for`) registers before it checks, on a versioned latch, so a
change between the check and the wait still wakes it.

## Bootstrap Names

Dynamic references carry resolved addresses and die with their incarnation.
Bootstrapping needs the opposite: a fixed service at a name that survives
restarts. `register_well_known::<M>(WellKnownId::new(1), ..)` registers one.
Its admission ignores the incarnation, so a `WellKnownRef` keeps working
across server restarts. A `BootstrapClient` resolves the `host:port` through
a `moonpool_core::Resolver`, caches the answer for a TTL, and
`retry_get_reply` retries under an explicit `RetryPolicy`: always after a
failure proven `NotAdmitted`, after an ambiguous one only if
`retry_ambiguous` says so, and never after a contract error.

## Interfaces as Data

A service with several methods is one **endpoint group**: one registry slot,
one incarnation, several methods told apart by their explicit ids.

```rust,ignore
struct Kv;
impl RpcInterface for Kv {
    const INTERFACE: InterfaceId = InterfaceId::new(0x6b76_0001);
    const VERSION: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "kv";
}
impl InterfaceMethod<Kv> for Get {}
impl InterfaceMethod<Kv> for Put {}

let group = rpc.register_group::<Kv>(AccessClass::Public)?;
let mut gets = group.serve::<Get>()?;   // a RequestStream<Get>
let mut puts = group.serve::<Put>()?;
let kv_ref: InterfaceRef<Kv> = group.interface_ref();
```

`kv_ref.method::<Put>()` gives a `ServiceRef<Put>` with the same address,
incarnation and token. A method the group does not serve is refused with
`MethodNotFound` before any handler runs. A reference to another interface is
refused with `InterfaceMismatch` before decoding. Both reference types are
protobuf messages with fixed tags, pinned by golden fixtures, so they are
ordinary data: a field of a request, a reply, a file on disk. A decoded
reference is checked before use, and one that names another method or
interface fails with `InvalidReference` before anything leaves.

**Restarts are learned, never refreshed.** When a process restarts at the
same `ip:port`, every reference to its previous incarnation is refused with
`StaleIncarnation` and never dispatched to the new boot, not even by a
reliable call. The new boot publishes new references and callers learn them
through the application: a well-known directory, a recruitment reply. That is
FoundationDB's model too. A fresh endpoint says nothing about whether the
durable state behind it is the one we were talking to.

A `ReplyHandle` is bound to the session its request arrived on and has no
byte form, so it cannot be forwarded. A callback a third party may call is an
ordinary registered endpoint whose `ServiceRef` travels in the request.

## Balancing Across Alternatives

A read that three replicas can serve should go to the fastest one, skip the
one that just crashed, and maybe try a second one when the first is slow.
`moonpool_rpc::balance` keeps FoundationDB's `loadBalance` mechanics and
makes the permissions explicit, because **sending one request twice is only
safe if the application says so**:

```rust,ignore
let model = QueueModel::new(ModelConfig::default())?;
let balanced = BalancedClient::new(&rpc, set, model.clone(), BalanceConfig {
    locality: Locality::new("m1", "dc1"),
    ..BalanceConfig::default()
})?;
spawn(model.collect_lagging()); // late losers still update the model

// An idempotent read: retry after an ambiguous attempt, hedge a slow one.
let policy = BalancePolicy {
    retry: Retry::AfterAmbiguous,
    duplicates: Duplicates::hedged(),
    ..BalancePolicy::default()
};
let read = balanced.call(Get { key }, &policy).await?;
```

The input is an `AlternativeSet<M>`: typed references to specific
incarnations, each with a `Locality`, under a `SetVersion` the application
chooses. The balancer never refreshes a reference. The application installs
a newer set with `replace`, and a call whose every alternative is gone fails
with `StaleAlternatives`. Two permissions are independent and off by default:
`Retry::AfterAmbiguous` and `Duplicates::Permitted`. Failing over after an
attempt proven `NotAdmitted` needs no permission. The `balanced_mutation`
example shows the difference on a deposit whose reply is lost: at most once
reports `MaybeExecuted` and stops, a blind retry deposits twice, a deposit
keyed by id applies once.

## Shutting Down

Dropping the driver is the abrupt shutdown. `RpcHandle::shutdown(grace)` is
the graceful one: admission closes (peers get `ServerShuttingDown`, local
calls `Shutdown`), admitted work may finish within `grace` of provider time,
and the rest ends with the execution knowledge it has. The returned
`ShutdownReport` says whether the work drained. `RpcHandle::stats()` is a
snapshot of every counter and gauge, and `observability::RpcMetrics` exports
it as a `MetricsSource` with closed label sets, so a million peers add no
series.

## In a Simulation

Inside a `Process`, the driver is polled next to the handler with
`moonpool_sim::select!`, so a crash drops both:

```rust,ignore
let (driver, rpc) = RpcDriver::listen(ctx.providers().clone(), &address, config).await?;
let (echo_ref, mut requests) = rpc.register::<Echo>(AccessClass::Public)?;
ctx.state().publish("echo_ref", echo_ref);   // callers learn it from the application
moonpool_sim::select! {
    error = driver.run() => Err(invalid_state(format!("rpc driver: {error}"))),
    () = serve(&mut requests) => Ok(()),
}
```

The runtime draws its incarnation, jitter and backoff from the
`RandomProvider`, and every timeout from the `TimeProvider`, so a seed replays
the same session history every time. What the runtime promises, and how the
`moonpool-rpc-sim` campaigns check it, is the next chapter:
[What moonpool-rpc Promises](./03-rpc-guarantees.md). Admission budgets,
security and the derive macro follow in
[Admission, Security and Derive](./04-rpc-operations.md).
