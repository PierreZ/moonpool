# Moonpool Transport DX Refactor — Implementation Guide

## Goal

Make moonpool's RPC ergonomics match FDB's fdbrpc. After this round: define an interface trait → `transport.serve::<T>()` or `transport.client::<T>(addr)` → call methods on the result. No `&transport` threading, no codec generics on service types, no turbofish, no `time.timeout` wrapping RPCs.

## Pre-reading

- `docs/analysis/foundationdb/layer-3-fdbrpc.md` — sections 2, 4, 5, 6, 7, 8, 9, 14
- `moonpool-transport/examples/ping_pong.rs` and `calculator.rs` — current DX gaps
- `moonpool-transport-derive/src/lib.rs` — the `#[service]` proc macro
- `moonpool-transport/src/rpc/failure_monitor.rs` — existing failure monitor

## Working Rules

- Land tasks **one at a time**. Each task = one commit, conventional commits with `!` for breaking changes.
- After each task: `cargo fmt`, `cargo clippy -- -D warnings`, `cargo nextest run`, run relevant examples.
- After each task: update examples AND book. Stale book sections are not acceptable.
- Do not batch tasks or "while I'm in here" refactors.
- Tasks ordered by dependency.

## Cross-Cutting Invariants

1. `ServiceEndpoint<Req, Resp>` must remain cheap-clone and placeable in collections.
2. Single-threaded runtime: `current_thread` tokio, `Rc`/`RefCell` over `Arc`/`Mutex`.
3. `TaskProvider` trait stays — it's the simulation seam.

---

## Task 0 — Remove load-balance and fan-out

**Rationale:** These couple to the current `ServiceEndpoint<Req, Resp, C>` shape. Will be rebuilt from scratch against the post-refactor API.

**Remove:**
- `moonpool-transport/src/rpc/load_balance.rs` (LoadBalanceConfig, Alternatives, QueueModel, Distance, AtMostOnce, ModelHolder, load_balance fn)
- `moonpool-transport/src/rpc/fan_out.rs` (fan_out_quorum, fan_out_all, fan_out_race, and all helper types)
- Examples: `load_balanced_reads.rs`, `fan_out_quorum_demo.rs`, `fan_out_all_demo.rs`, `fan_out_all_partial_demo.rs`, `fan_out_race_demo.rs`
- `[[example]]` entries in `moonpool-transport/Cargo.toml`
- Re-exports from `moonpool-transport/src/rpc/mod.rs` and `moonpool-transport/src/lib.rs`
- Tests exercising load-balance or fan-out. Rewrite any that tested something else using these as a vehicle.

**Book:** Remove or stub `book/src/part4-networking/11-load-balance-fan-out.md`. Add note: "Load balancing and fan-out primitives will be reintroduced in a future revision against the new transport API."

**Acceptance:** `rg 'load_balance|fan_out|Alternatives|QueueModel|LoadBalanceConfig|AtMostOnce' moonpool-transport/` returns no live code. `cargo build --all-targets` passes. `ping_pong.rs` and `calculator.rs` still run.

**Commit:** `chore!: remove load_balance and fan_out primitives`

---

## Task 1 — Remove simulation code from transport

**Rationale:** Simulation paths will be rewritten from scratch against the cleaned-up API. Migrating them through this refactor costs more than rewriting.

**Remove:**
- All `buggify_with_prob!()` invocations (~12 sites: `peer/config.rs`, `peer/core.rs`, `rpc/net_transport.rs`)
- All `assert_always!`, `assert_sometimes!`, `assert_reachable!` invocations (~107 sites across: `failure_monitor.rs`, `endpoint_map.rs`, `delivery.rs`, `reply_promise.rs`, `net_transport.rs`, `peer/core.rs`)
- The `moonpool-sim` dependency from `moonpool-transport/Cargo.toml`
- Any `cfg(simulation)` / `cfg(feature = "simulation")` conditional code
- Any simulation transport implementations (if separate from real TCP path)

**Keep:**
- `moonpool-transport/src/rpc/test_support.rs` — this is standard unit test infrastructure (MockNetworkProvider using real providers), NOT simulation code
- The `TaskProvider` trait — it's the simulation seam, not simulation itself
- Real TCP transport, `TokioProviders`, `JsonCodec`, `#[service]` macro, `FailureMonitor`, both examples
- `moonpool-transport/src/rpc/smoother.rs` — pure math utility

**Book:** Remove/stub chapters documenting simulation features. Add note: "Deterministic simulation will be reintroduced in a future revision against the new transport API."

**Acceptance:** `rg -i 'buggify|assert_always|assert_sometimes|assert_reachable' moonpool-transport/` returns nothing. `cargo build --all-targets` passes. All remaining tests pass.

**Commit:** `chore!: remove simulation code from transport`

---

## Task 2 — Allocate endpoint tokens dynamically per server instance

**Current state:** `#[service(id = 0xCA1C_0000)]` uses hardcoded ID as high bits → every instance has same tokens. Makes all services behave like FDB well-known endpoints.

**Change — Two tiers:**

### Tier A: Dynamic (default)
```rust
#[service]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
}
```
- `transport.serve::<Calculator>()` allocates fresh random UID for first method, adjacent UIDs (offset+1, offset+2...) for rest
- Interface holds runtime tokens — meaningful only if serialized and shipped to client (Task 6)
- `Calculator::new(addr, codec)` **removed** — clients obtained only via serialization or shared registry

### Tier B: Well-known (opt-in)
```rust
let iface = transport.serve_well_known::<PingPong>(WLTOKEN_PING);
let client = transport.client_well_known::<PingPong>(addr, WLTOKEN_PING);
```
- `WellKnownToken` newtype around `NonZeroU64` from reserved range
- `WLTOKEN_RESERVED_COUNT = 64` — panic if registration outside range
- Central registry in `moonpool-transport/src/well_known.rs`

**Files to modify:**
- `moonpool-transport-derive/src/lib.rs` — drop required `id` attr, remove `INTERFACE_ID` const (or make optional)
- `moonpool-transport/src/rpc/net_transport.rs` — add `serve::<T>()`, `serve_well_known::<T>(token)`, `client_well_known::<T>(addr, token)` methods with token allocation
- `moonpool-transport/src/well_known.rs` (new or update existing `moonpool-core/src/well_known.rs`)

**Examples:**
- Rename `ping_pong.rs` → `well_known_endpoint.rs` using `serve_well_known`/`client_well_known`
- Rename `calculator.rs` → `dynamic_endpoint.rs` using `serve()` + shared `Rc<RefCell<Option<Calculator>>>` registry

**Acceptance:** Two `Calculator` servers in same process don't collide. Both examples run end-to-end. ServiceEndpoint cheap-clone preserved.

**Commit:** `refactor!: dynamic endpoint tokens with well-known opt-in`

---

## Task 3 — Hold the transport inside the interface

**Current state:** Every RPC call passes `&transport`:
```rust
calc.add.get_reply(&transport, request)
stream.recv_with_transport::<_, Resp>(&transport)
```

**Change:** Bind transport at construction. Interface objects hold `Rc<NetTransport<P>>` internally.

**API target:**
```rust
// Server
let (req, reply) = iface.add.recv().await;       // no &transport, no turbofish

// Client
let resp = calc.add.get_reply(req).await?;       // no &transport
```

**Files to modify:**
- `moonpool-transport/src/rpc/service_endpoint.rs` — add transport field, remove `&transport` from `send()`, `try_get_reply()`, `get_reply()`, `get_reply_unless_failed_for()` signatures
- `moonpool-transport/src/rpc/request_stream.rs` — rename `recv_with_transport` to `recv`, remove transport param
- `moonpool-transport-derive/src/lib.rs` — generated structs carry transport handle, `serve()` passes it
- `moonpool-transport/src/rpc/delivery.rs` — adjust function signatures

**Acceptance:** Zero `&transport` inside `.get_reply()`, `.send()`, `.recv()` args in examples and book. ServiceEndpoint cheap-clone preserved (cloning duplicates the Rc).

**Commit:** `refactor!: bind transport at interface construction`

---

## Task 4 — Erase codec generic from user-facing types

**Current state:** `CalculatorClient<C: MessageCodec>` forces turbofish everywhere.

**Change:** Introduce object-safe `TransportHandle` trait. `NetTransport<P>` implements it, performing serialization internally. Service types hold `Rc<dyn TransportHandle>`.

```rust
pub trait TransportHandle {
    fn send_unreliable(&self, endpoint: &Endpoint, payload: &[u8]) -> Result<(), MessagingError>;
    fn send_reliable(&self, endpoint: &Endpoint, payload: &[u8]) -> Result<(), MessagingError>;
    fn register_handler(&self, token: UID) -> /* receiver */;
    fn failure_monitor(&self) -> Rc<FailureMonitor>;
    fn local_address(&self) -> &NetworkAddress;
    fn allocate_token(&self) -> UID;
    // ... sleep for on_failed_for
}
```

**Key insight:** `NetTransport<P>` already encapsulates both P (providers) and C (codec) internally. The `P` generic serves the simulation seam but users never need to name it. Erasing both P and C behind `dyn TransportHandle` is correct.

**Codec moves to transport construction only:**
```rust
let transport = NetTransportBuilder::new(providers)
    .local_address(addr)
    .codec(JsonCodec)
    .build_listening()
    .await?;
```

**Files to modify:**
- `moonpool-transport/src/rpc/service_endpoint.rs` — `ServiceEndpoint<Req, Resp>` (no C)
- `moonpool-transport/src/rpc/request_stream.rs` — `RequestStream<Req>` (no C)
- `moonpool-transport/src/rpc/reply_promise.rs` — `ReplyPromise<T>` (no C)
- `moonpool-transport/src/rpc/net_transport.rs` — implement TransportHandle, add codec field to builder/transport
- New: `moonpool-transport/src/rpc/transport_handle.rs` — trait definition
- `moonpool-transport-derive/src/lib.rs` — remove `<C>` from emitted types

**Acceptance:** `cargo expand` on macro output shows no `<C: MessageCodec>` on user-facing structs. Examples have exactly one `JsonCodec` mention (in builder). Zero turbofish.

**Commit:** `refactor!: erase codec generic from user-facing types`

---

## Task 5 — Unify Server and Client into single interface type

**Current state:** Macro emits `CalculatorServer<C>` and `CalculatorClient<C>`. FDB uses one `StorageServerInterface` for both.

**Change:** Single struct per service. Construction determines mode:
```rust
let iface = transport.serve::<Calculator>();                           // local mode (recv works)
let iface = transport.client_well_known::<PingPong>(addr, token);     // remote mode (get_reply works)
```

**Design:** Single field type per method with `is_remote_endpoint()` predicate. Server methods (`recv()`) work when local; client methods (`get_reply`) work when remote. Runtime check — panic or error if called in wrong mode.

**Files to modify:**
- `moonpool-transport-derive/src/lib.rs` — emit one struct, merge Server/Client field types
- `moonpool-transport/src/rpc/service_endpoint.rs` + `request_stream.rs` — may unify into single bidirectional handle type
- `moonpool-transport/src/rpc/mod.rs` — update re-exports

**Acceptance:** `CalculatorServer` and `CalculatorClient` no longer exist. Both examples use one type (`Calculator` / `PingPong`). ServiceEndpoint cheap-clone preserved.

**Commit:** `refactor!: unify Server and Client into single interface type`

---

## Task 6 — Serializable interfaces with endpoint adjustment

**Current state:** ServiceEndpoint already derives Serialize/Deserialize. Missing: compact serialization (address once + base token, offsets derived).

**Change:** Custom Serialize/Deserialize on unified interface:
1. Serialize: address once + first field's full token. Subsequent fields = token + offset.
2. Deserialize: reconstruct each field's endpoint via offset. Result is remote-mode.
3. Demonstrate round-trip in `dynamic_endpoint.rs`:
```rust
let iface = transport.serve::<Calculator>();
let bytes = serde_json::to_vec(&iface)?;
let received: Calculator = serde_json::from_slice(&bytes)?;
*registry.borrow_mut() = Some(received);
```

**Files to modify:**
- `moonpool-transport-derive/src/lib.rs` — emit custom Serialize/Deserialize impls with adjustment
- `moonpool-transport/examples/dynamic_endpoint.rs` — add serialize round-trip

**Book:** New chapter: "Interfaces are data" — endpoint adjustment, FDB section 7 analogy.

**Acceptance:** Test: construct server interface → serialize → deserialize → make RPCs. Serialized form smaller than N independent endpoints. `dynamic_endpoint.rs` includes round-trip.

**Commit:** `feat: serializable interfaces with endpoint adjustment`

---

## Task 7 — Verify broken_promise on reply handle drop (ALREADY IMPLEMENTED)

**Current state:** `ReplyPromise::drop()` in `reply_promise.rs` already sends `BrokenPromise` if `!self.consumed`. The `ReplyFuture` client-side handles this correctly.

**Scope:** Verify-only. No code changes needed.

**Verify:**
- `tests/rpc_integration.rs` has `test_broken_promise` covering the server-drop → client-receives-error path
- Document drop semantics in rustdoc on reply handle and reply future
- Add book chapter: "Drop semantics and the WaitFailure pattern"

**If missing:** Write integration test: server receives request, drops reply handle → client's `get_reply` resolves to `Err(BrokenPromise)`.

**Acceptance:** Test exists and passes. Drop semantics documented.

**Commit:** `docs: document broken_promise drop semantics` (or skip if already documented)

---

## Task 8 — Verify delivery modes (ALREADY IMPLEMENTED)

**Current state:** `ServiceEndpoint` already has all four delivery modes:
- `send()` — fire-and-forget (service_endpoint.rs)
- `try_get_reply()` — at-most-once (delivery.rs)
- `get_reply()` — at-least-once (delivery.rs)
- `get_reply_unless_failed_for()` — at-least-once + failure timeout (delivery.rs)

**Scope:** Verify-only. Ensure examples demonstrate at least 3 modes and no `time.timeout` wraps RPCs.

**Verify after Tasks 2-5:**
- Examples use `get_reply_unless_failed_for` where timeouts are needed (not `time.timeout`)
- At least one example demonstrates `send()` (fire-and-forget)
- Book "Delivery modes" chapter is accurate

**Acceptance:** All four modes on ServiceEndpoint. Examples demonstrate at least 3. Zero `time.timeout` wrapping RPCs.

**Commit:** Part of example/book updates in earlier tasks (no separate commit needed)

---

## Task 9 — Promote on_failed_for to FailureMonitor public API

**Current state:** `on_failed_for()` exists as a private async function in `delivery.rs` (lines 219-228). It calls `fm.on_disconnect_or_failure()` then `time.sleep(duration)`. It is NOT a method on `FailureMonitor`.

**Change:** Move to public method on `FailureMonitor`:
```rust
impl FailureMonitor {
    pub async fn on_failed_for(&self, endpoint: &Endpoint, duration: Duration) {
        self.on_disconnect_or_failure(endpoint).await;
        self.time.sleep(duration).await;
    }
}
```

**Requires:** `FailureMonitor` holds (or is passed) a `TimeProvider` handle. Currently it does not. Add `Rc<dyn TimeProvider>` field at construction.

**Note:** The current private helper does NOT restart on recovery (step 3 of original plan: "if state flips back to Available, restart from step 1"). Decide whether to add retry-loop semantics or keep the simple version. The simple version (sleep after first failure detection) matches what `get_reply_unless_failed_for` currently uses.

**Files to modify:**
- `moonpool-transport/src/rpc/failure_monitor.rs` — add `on_failed_for()` method, add TimeProvider field
- `moonpool-transport/src/rpc/delivery.rs` — replace private helper with `fm.on_failed_for()` call
- `moonpool-transport/src/rpc/net_transport.rs` — pass TimeProvider when constructing FailureMonitor

**Acceptance:** `on_failed_for` available on FailureMonitor. delivery.rs uses it. Test: poison server address → `get_reply_unless_failed_for(100ms)` → `Err(MaybeDelivered)` after ~100ms.

**Commit:** `feat: add on_failed_for to failure monitor`

---

## Final Book Audit (after all tasks)

- Every code sample compiles against current API
- Chapters referencing load-balance, fan-out, simulation are rewritten or deleted
- TOC reflects: well-known vs dynamic endpoints, four delivery modes, failure monitor, drop semantics, interface transmission
- Add "What's next" section noting load-balance, fan-out, and simulation return in future revisions

**Commit:** `docs: post-refactor book audit and rewrite`

---

## Final "Dream" Shape

After all tasks, `well_known_endpoint.rs`:
```rust
#[service]
trait PingPong {
    async fn ping(&self, req: PingRequest) -> Result<PingResponse, RpcError>;
}

async fn run_server(transport: Rc<NetTransport>) -> Result<()> {
    let iface = transport.serve_well_known::<PingPong>(WLTOKEN_PING);
    loop {
        let (req, reply) = iface.ping.recv().await;
        reply.send(PingResponse { seq: req.seq, echo: format!("pong: {}", req.message) });
    }
}

async fn run_client(transport: Rc<NetTransport>) -> Result<()> {
    let iface = transport.client_well_known::<PingPong>(SERVER_ADDR, WLTOKEN_PING);
    for seq in 0..5 {
        let resp = iface.ping.get_reply(PingRequest { seq, message: format!("hello {seq}") }).await?;
        println!("got {resp:?}");
    }
    Ok(())
}
```

And `dynamic_endpoint.rs`:
```rust
#[service]
trait Calculator {
    async fn add(&self, req: AddRequest) -> Result<AddResponse, RpcError>;
    async fn sub(&self, req: SubRequest) -> Result<SubResponse, RpcError>;
}

async fn main_demo(transport: Rc<NetTransport>) -> Result<()> {
    let registry: Rc<RefCell<Option<Calculator>>> = Rc::new(RefCell::new(None));

    let server_transport = transport.clone();
    let server_registry = registry.clone();
    spawn_task("calc_server", async move {
        let iface = server_transport.serve::<Calculator>();
        let bytes = serde_json::to_vec(&iface).unwrap();
        let received: Calculator = serde_json::from_slice(&bytes).unwrap();
        *server_registry.borrow_mut() = Some(received);
        // handle requests via iface...
    });

    let calc = registry.borrow().clone().expect("interface published");
    let resp = calc.add.get_reply(AddRequest { a: 2, b: 3 }).await?;
    println!("2 + 3 = {}", resp.value);
    Ok(())
}
```

No `&transport` in call sites. No codec generics. No turbofish. No `time.timeout`. Reader sees the FDB shape.
