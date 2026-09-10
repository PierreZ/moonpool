# moonpool-rpc architecture

Status: approved design direction; implementation is organized by the linked GitHub packages. No RPC code or placeholder crates are introduced by this planning change. See [scope and execution order](README.md) and [behavioral coverage](parity.md).

## Decisions from the clarification checkpoint

The user accepted typed references with explicit request receivers/reply handles, Rust-defined messages with an established codec and explicit schema evolution, qualification on only Moonpool-supported platforms/providers and its existing wasm protocol/simulation path, internal fairness/control reserves instead of a Flow priority scheduler, and prompt terminal errors for definitively stale dynamic endpoints. They explicitly deferred **mTLS**. Server-authenticated TLS, per-request verification and a JWT/JWKS adapter remain in the accepted proposal; mutual certificate authentication of clients is not a release gate. No internet-scale multi-tenant service commitment is inferred.

## Crates and dependency direction

Keep `crates/` flat. `moonpool-rpc` owns `protocol/`, `transport/`, `endpoint/`, `call/`, `stream/`, `failure/`, `balance/`, `security/`, and `observability/` modules. Production depends on `moonpool-core`, established protocol/security dependencies, `tracing` and lightweight instrumentation where useful. It must never depend on `moonpool-sim` or explorer, including through feature defaults. `moonpool-rpc-sim` is non-published and owns workloads, independent oracles, fault campaigns and simulation integration. `moonpool-rpc-derive` is optional ergonomics over the manual API; its macros do not own registry, protocol, delivery or retry semantics. No crate per C++ component.

Expose an optional `rpc` facade feature after operational integration. `moonpool-hyper` stays independent; native RPC uses provider TCP directly, not HTTP/2. Declining the feature must not grow unrelated consumers' dependency trees. Public RPC crate features and harness registration are introduced by implementation packages, not scaffolding in this plan.

## Provider integration and justified lower-layer changes

Use static `P: Providers`, associated stream types implementing `futures::io`, `Send + 'static` futures and AFIT provider contracts. Production works on Tokio's current-thread and multithreaded runtimes; simulation runs on Moonpool's seeded executor. No ambient runtime globals, `spawn_local`, direct Tokio timers/spawn/select or simulation dependency in RPC. Use `moonpool_core::select!`: core is the production re-export boundary and simulation feature unification installs deterministic branch offsets.

| Need | Existing capability and decision | Owner |
|---|---|---|
| TCP I/O | NetworkProvider bind/connect, listener accept/address; generic stream I/O. No new RPC transport trait in core. | [#213](https://github.com/PierreZ/moonpool/issues/213) |
| Scheduling time | TimeProvider sleep/timeout/now; `timer()` is application drift time. Define RPC timer use explicitly: retry/backoff/credit/latency decisions use provider scheduling time, never host wall time. | [#213](https://github.com/PierreZ/moonpool/issues/213), [#214](https://github.com/PierreZ/moonpool/issues/214) |
| Random choices | RandomProvider for protocol incarnation draws, retry jitter, selection and testable choices; ordered state iteration avoids hidden entropy. | [#213](https://github.com/PierreZ/moonpool/issues/213), [#217](https://github.com/PierreZ/moonpool/issues/217) |
| Task lifetime | TaskProvider handles detach on drop and expose no generic abort. Prefer a caller-owned driver future that owns/polls children; cancellation drops the whole driver tree. If a child must be spawned, wrap it with explicit abort/drop ownership and account for shutdown completion. Prove crash cancellation before expanding core. | [#213](https://github.com/PierreZ/moonpool/issues/213), [#216](https://github.com/PierreZ/moonpool/issues/216) |
| DNS/cache invalidation | `connect(&str)` cannot expose resolved alternatives, refresh or scripted DNS outcomes. Add a separate optional generic resolver capability, reusable by non-RPC clients, without adding an obligatory Providers associated type. RPC bootstrap/retry policy stays in RPC; scripted harness implementation supplies deterministic DNS. | [#214](https://github.com/PierreZ/moonpool/issues/214) |
| Trust/peer metadata | Listener returns a peer address, which is not authenticated identity. Security adapter supplies explicit peer/principal context; no core RPC authorization types. | [#218](https://github.com/PierreZ/moonpool/issues/218) |
| UTC and crypto entropy | Scheduling clock is not Unix/certificate time, and RandomProvider has no CryptoRng contract. Supply UTC validation inputs and dependency-owned cryptographic entropy at the security boundary. Core extension requires demonstrated general reuse. | [#218](https://github.com/PierreZ/moonpool/issues/218) |
| Priorities | No provider priority API. RPC owns ordering, bounded work per poll, admission and control reserves. No core scheduler expansion is planned. | [#216](https://github.com/PierreZ/moonpool/issues/216) |
| Metrics | MetricsSource is a scrape seam, not a new runtime selector. Use provider-time measurements for model decisions and deterministic metrics; host performance measurements stay outside semantic oracles. | [#217](https://github.com/PierreZ/moonpool/issues/217), [#218](https://github.com/PierreZ/moonpool/issues/218) |

The user explicitly authorized improving Moonpool while implementing RPC. Keep those improvements in the package that needs them; no extra approval gate is needed for an in-scope fix. Support only what Moonpool supports: use its existing provider/platform/feature matrix as the source of truth, with no additional platform promise. Every proposed core change must include a non-RPC use case, production and simulated behavior, feature isolation, and book/portability checks. Endpoint, EndpointToken, RequestStream, reply handles, RpcError, protocol versions and failure-monitor state belong exclusively in RPC.

## Endpoint identity and interfaces as data

A dynamic reference identifies a particular receiving endpoint within a particular RPC transport incarnation, plus enough network address information to reach it. Conceptually it includes address information, incarnation, endpoint/group identity, method identity and schema identity. This is a semantic model, not a prescribed UID layout or speculative public struct definition.

Use fresh provider-random incarnation material across boots, checked registry generations and non-wrapping allocation. Detect live collisions/reuse, reject exhausted generations, and test a forced registry-slot reuse. Token uniqueness is probabilistic across independently started processes unless an application provides a stronger persistent identity source; use adequate width and document that assumption. Tokens are not security credentials. Reboot and registry reuse must never cause deterministic ABA aliasing.

Well-known endpoints are an explicit separate addressing class for bootstrap and may recover at the same address/token. Dynamic endpoints never refresh themselves. Every admission validates the incarnation, endpoint identity and schema/method contract before invoking user code. Destroying a receiver/group rejects new admissions; already admitted requests may finish only under an owned lifetime guard, with the policy documented and bounded at shutdown.

A service interface is immutable serializable routing data, independent of a live registry or runtime. Users may return it in a reply, embed it in a request, persist it or forward it to another participant that can reach the advertised address. Binding it to a runtime creates a client handle; it does not register a new local receiver or verify durable application identity. Remote clones do not keep the server alive. Multiple runtime-created services in one process remain independent.

Grouped endpoints compactly share routing/incarnation metadata. Method adjustment uses checked stable identifiers and validates membership; declaration reordering cannot change existing wire meaning. Well-known registration and ordinary service groups have separate lifecycle rules. Macros simplify definitions after the manual Paros example works.

### Reply routes and general callbacks

FDB ReplyPromise serialization writes only a token; `loadedEndpoint` supplies the current delivery peer. Preserve that useful connection-relative concept with an explicit decode context, not a process/thread-global current peer. A request's reply route is bound to the peer/session incarnation that delivered it, including connection-generation failure observation. It must not be reserialized into a request to an arbitrary third peer as if globally routable.

Forwardable callbacks use ordinary fully addressed dynamic endpoints. Reply handles are one-shot ownership objects, not automatically general service references. Repeated reliable request execution may construct more than one responder to the same logical caller; the caller accepts one completion and safely discards later responses. The exact route behavior after connection loss must prohibit delivery to a replacement client process at the same address.

## Ownership, cancellation and process isolation

An RPC runtime owner controls listener(s), endpoint registry, peers, child I/O drivers, pending calls, streams, failure subscriptions and model accounting. Client references access this state weakly or through a closing gate; they must not keep the owner serving after it is dropped. Per-instance ownership replaces FDB's simulation-scoped globals.

Moonpool aborts the root process task and its network connections on a crash; TaskProvider child tasks are not automatically a structured subtree. Construct lifetime guards before a child's first poll, arrange driver-owned cancellation, and prove no child binds/reconnects/dispatches after the owning process dies. Sim providers are scoped by IP, not a documented process-incarnation capability; do not rely on an old provider clone becoming invalid automatically. Driver ownership and admission gates must make it unusable after crash. If testing proves a generic provider-generation safeguard necessary, justify it explicitly rather than silently relying on a new simulation guarantee.

Dropping a caller unregisters its reply route and removes future retransmission retention. It does not retract bytes, cancel server effects or prove non-execution. An explicit no-reply completion differs from accidentally dropping a responder (broken promise). All cancel/shutdown paths wake waiters and release reservations once. Graceful shutdown closes admission, drains within a provider-time deadline and then aborts remaining work; abrupt drop closes immediately. Successful local flushing alone is not a remote execution/durability acknowledgement.

## Transport and delivery semantics

Incremental bounded framing validates length, schema/protocol and integrity before dispatch. Partial I/O, several frames per read and zero-write/EOF errors are normal inputs. A peer driver owns handshake, connection selection, read/write progress, ping, idle/reference accounting and reconnect with jitter and stable-uptime backoff reset. Simultaneous connections select one deterministically and close losers. FDB uses address ordering with exceptions, not the retained analysis's invented larger-connection-ID rule.

Maintain two concepts: unsent byte buffers with write cursors, and logical reliable requests retained for possible retransmission. Disconnect discards an obsolete byte stream and reconstructs transmissions from still-owned reliable requests. Cancelling retention does not remove bytes already in a current stream. Replies, streaming items and ACKs have their own one-attempt lifetime; there is no durable retransmission log or transparent duplicate suppression.

| API family (names illustrative) | Contract | Completion/failure |
|---|---|---|
| send | One-way, zero-or-one delivery per admitted attempt; no automatic replay. | Local admission error is observable; successful enqueue is not execution confirmation. |
| tryGetReply equivalent | One request attempt; no hidden retransmission. | Reply, explicit error or ambiguous timeout/disconnect; cancellation may follow execution. |
| getReply equivalent | Retain request while owned; resend on reconnect; possible repeated execution. | First reply wins; cancellation/shutdown releases retention; terminal stale endpoint returns promptly. |
| getReplyUnlessFailedFor equivalent | Reliable mode additionally bounded by permanent/sustained observed failure. | Preserves ambiguity. Sustained failure is not a wall deadline or proof of death. |
| getReplyStream equivalent | One registration attempt followed by ordered replies and consumption credit. | Explicit end/error/cancel or disconnect; no transparent resumption. |

Definitive dynamic failure differs deliberately from FDB's `sendCanceler`, which may cancel retention and then wait forever. Return terminal stale-endpoint information promptly, without erasing knowledge that an earlier attempt may have executed. Error categories separate reason (transport, lookup, stale, unauthorized, overload, protocol, broken promise, timeout, cancellation) from execution knowledge. Authorization/overload rejection proves non-admission only for the rejected attempt, not previous attempts.

## Failure monitoring and retries

Keep address availability, connection-generation disconnect events and endpoint-specific failure separate. An address can recover while an old dynamic endpoint remains dead. A newly connected peer does not make all old endpoint references valid. Well-known recovery is explicit. Authorization failure remains distinct from not-found, and policy/key changes need a documented invalidation path rather than permanent accidental lockout.

Subscriptions register before checking state and use a revision or equivalent latch so no state change is lost. Calls watch the actual connection generation as well as delayed address failure; an event need not change the address boolean. Bound failed-endpoint caches without compromising stale-token validation. Sustained-failure helpers reset on recovery and define duration/slope bounds.

Bootstrap DNS and well-known retry helpers may refresh addresses under an explicit retry policy. This is not dynamic service refresh. The application decides whether retrying an ambiguous operation is safe; concurrent copies require separate permission.

## Streaming, scheduling and resource control

Each stream has an ordered sequence, explicit terminal state, ACK route and checked cumulative sent/consumed-byte accounting. The producer waits for readiness based on unconsumed bytes; the client acknowledges when an item is handed to a waiting consumer or popped from its queue. Reading socket bytes alone does not earn application consumption credit. Define byte units using encoded/bounded item sizes, reserve credit atomically and handle oversized items explicitly. FDB's `expectedSize()` estimate and per-send readiness are references, not mandatory Rust accounting rules.

Cancellation before the first ACK route arrives, abandonment, late ACKs, sequence errors, exhausted credit and both sides of disconnect need explicit transitions. Completion and error ordering are protocol-level rules, not accidents of a local bounded channel. A new stream after reconnect is application action.

Reserve capacity and bounded dispatch for ACK, cancel, ping, terminal errors and shutdown. Never park the socket reader on a full user queue. Cap frame and batch size to limit head-of-line blocking; classify traffic fairly without inventing global Flow priorities. Admission and retained-byte budgets apply per endpoint, peer and runtime, including reliable requests, late losers, decode allocations and incomplete handshakes. Report overload at the boundary where rejection occurs. Numerical defaults are measured and frozen in the owning issue, with explicit saturation/recovery evidence.

## Load balancing

Alternative sets carry typed incarnation-specific references and explicit freshness/version information. A generic locality descriptor ranks same machine, same datacenter and distant choices, without putting simulation topology types in production. Model outstanding weighted work, observed latency, server penalties and temporary exclusion/backoff using provider time. Track each attempt through a reservation released exactly once.

Retry permission and concurrent-duplicate permission are separate. Hedging consumes a bounded budget and uses measured delay; winning a race does not discard accounting for the loser. Bounded lagging-response collection records late outcomes and releases model reservations even after cancellation. Generic hooks may classify temporary-behind/overload results or compare explicitly permitted duplicate responses. FDB storage versions, TSS business logic, replica membership and consensus stay application responsibilities.

## Security and evolution boundary

mTLS is deferred by the user's answer, not silently marked complete. Server-authenticated TLS plus verified request credentials can establish server and client principals respectively. IP allowlists restrict reachability but do not authenticate identities; private endpoints require explicit policy. Public endpoints remain subject to request verification and limits. Local calls pass equivalent policy checks. No self-asserted handshake identity grants trust.

Use maintained TLS/token libraries; implement no cryptographic primitives. Standard JWT/JWKS verification includes algorithm restrictions, signature, configured issuer/audience, expiry/not-before and rotation/cache invalidation. Applications own claim meaning, token issuance and permissions. Credentials are redacted from traces and metrics. No insecure automatic downgrade. Explicit plaintext simulation/trusted-deployment mode cannot claim confidentiality or cryptographic peer authentication.

Protocol RNG is for scheduling/jitter/IDs, cryptographic entropy belongs to vetted dependencies, TimeProvider is scheduling time, and real UTC is a separate security-validation input. Deterministic policy scenarios use explicit credentials/time inputs; real TLS/token tests qualify cryptographic interoperability separately and do not require identical ciphertext/RNG traces.

Version our transport and message/interface schemas. P1 selects an established codec with bounded decode and a documented evolution strategy; P6 proves supported adjacent-version rolling combinations and rejects unsupported ones. Stable method/schema identifiers cannot derive from Rust type names, compiler layout or method order. Golden fixtures include stored interface data. No Flow serializer, FDB layout, protocol history or FDB interoperability is a goal.

## Paros validation without membership

Keep `ParticipantId`, `ServiceInterface` and `ConfigurationId` as separate application types. A fixture holds durable participant A constant while A publishes I1; B learns I1 and successfully calls it. Crash A, restart it at the **same IP:port**, allocate/publish I2, and demonstrate I1 cannot dispatch to I2. B explicitly learns I2 and succeeds. Recruit D/E with separate interfaces and transmit them to a third participant. Repeat with delayed registration, cancelled calls and reliable ambiguity. A fresh endpoint is neither fencing nor evidence of durable-state continuity. The fixture drives recruitment/publication; RPC does not implement discovery, reconfiguration or Paxos.

## Validation and unresolved implementation choices

Tests arrive with each contract. Independent workload ledgers observe handler executions, publication identity, returned replies and consumed items; they must not calculate expected behavior by reading the transport registry/queues. SimWorld faults cover connection loss, partial I/O, partitions, overload, scheduling and reboots. Bit flips are a separately configured supported corruption experiment, never made-up message loss in healthy TCP. Require explicit scenario hits, RNG canary, semantic replay, bounded termination and recovery after chaos stops. Final qualification combines previously tested behavior.

Remaining choices are bounded implementation work, not approval gates: P1 selects/pins codec and exact frame/schema representation; P2 closes resolver API details; P4 measures numerical resource budgets/control scheduling; P6 chooses security dependencies and supported upgrade window; P7 records performance acceptance envelopes. Native crypto feature support on wasm is not promised: protocol and simulated policy must stay usable with native security features off. Stronger client certificate authentication remains deferred. GitHub issues own all steps and acceptance evidence; this document is not a second backlog.
