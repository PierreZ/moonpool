# What moonpool-rpc Promises, and What It Doesn't

<!-- toc -->

The previous chapter walked through the RPC layer feature by feature. This one is the contract in one place: what `moonpool-rpc` promises, what it deliberately does not, and how we know. Every promise below is backed by tests on real TCP and by deterministic simulation campaigns whose oracles never ask the transport what it thinks happened. The full evidence record (every behaviour row, the seeds, the mutation checks and the measurements) lives in the repository at `docs/plans/moonpool-rpc/qualification.md`.

## The Promises

**Every failure says what it proves.** An `RpcError` carries a reason and one of `NotAdmitted`, `MaybeExecuted` or `Executed`, and the runtime earns the claim: `NotAdmitted` means no handler saw that attempt, ever. A refusal of a reliable call's retransmission says `MaybeExecuted`, because an earlier copy may have run. The qualification campaign re-judges every call against a handler ledger once the network has settled, and a single refusal that turned out to have run fails the seed.

**One attempt is one attempt.** `try_get_reply`, `attempt` and `send` never retransmit behind our back: at most one execution per call. `get_reply` retains the request and sends it again on every new connection until an outcome arrives, so it may execute more than once, and the campaigns require that duplicate to actually happen.

**A reference names one incarnation, forever.** A process restarted at the same `ip:port` refuses its predecessor's references with `StaleIncarnation` (a version 1 caller sees `EndpointNotFound`, the only refusal its protocol has), promptly and before any handler runs, whatever carries the reference: a direct call, a reliable call retained across the restart, a reply stream, a balanced set, a stored blob or a callback forwarded through a third process. Learning the new incarnation is an explicit application step (a directory lookup, a republication), never a silent refresh.

**Streams are ordered, bounded and end exactly once.** Items arrive in order without gaps or repeats; a producer never runs further ahead of what the consumer's application took than its window; every item that arrived is observable before the one terminal outcome; an abandoned stream releases its producer.

**Balancing obeys its permissions.** A balanced call runs at most as often as its retry and duplicate permissions allow, only on incarnations of the set it was given, and every attempt reserves and releases the queue model exactly once, late losers included.

**Nobody unauthorized runs.** Private endpoints fail closed; a credential is verified before dispatch, on every route including local calls, and refused for what is actually wrong with it; key sets rotate by replacement and time comes from an injected UTC clock, never from the scheduling clock.

**Versions interoperate or refuse observably.** Adjacent protocol versions talk; an unsupported one is refused at the handshake, never admitted.

**Shutdown is honest.** `RpcHandle::shutdown(grace)` closes admission, drains within the grace on provider time and ends what is left with the execution knowledge it has.

**Nothing outlives its owner.** Dropping a driver releases its tasks, connections and state; every byte, window and owed reply a runtime accounts for is held by an owner and returned exactly once. `ResourceProbe::is_at_baseline` checks all of it after the runtime is gone, and the qualification campaign checks it for every process death and at the end of every run, repeated runs included.

**Service comes back.** After faults stop, the qualification campaign must regain every service (fresh publications, calls, a new stream, a balanced call, a credentialed call, the legacy peer, recruitment and callbacks through a third process) within a declared bound of simulated time.

## What It Does Not Promise

These are deliberate, not missing:

- **No exactly-once delivery, no durable queue, no duplicate suppression.** Ambiguity is reported, never hidden; an operation that must not run twice needs an application key.
- **No stream resumption.** A broken stream ends; a new one is the application's decision.
- **No transparent incarnation or endpoint refresh.** A stale reference fails; learning the new one is explicit.
- **No mutual TLS.** Servers are authenticated by TLS; clients by request credentials only. Client certificates were deferred by decision, and nothing claims otherwise.
- **No FoundationDB wire compatibility.** The protocol, envelope and codec are this crate's own.
- **No confidentiality or peer authentication in plaintext mode.** `SecurityConfig::trusted_network()` claims nothing; credentials go over plaintext only where each side opts in separately (the sender with `send_credentials_over_plaintext`, the receiver with `accept_credentials_over_plaintext`); otherwise a credential is withheld from an unauthenticated session (`CredentialWithheld`, never sent).
- **No internet-scale multi-tenant isolation.** Budgets bound resources per connection and runtime; they are not a tenant scheduler.
- **No platform beyond Moonpool's own.** Linux and macOS on Tokio (current-thread and multi-thread; qualified on Linux, macOS pending its first CI run); on `wasm32-unknown-unknown`, the protocol and the simulation only, without the native TLS/JWT adapters.
- **No performance envelope.** The measured numbers are calibration points on recorded hardware, not service-level objectives.

## How It Is Qualified

The `sim-rpc-qualification` campaign combines every other campaign's contracts in one run. Three verifying servers are restarted at their own address throughout: crashed and held down, rebooted in place, drained gracefully (sometimes on request, under held calls and a stream). Each boot serves a work instance, recruits fresh instances per configuration, produces reply streams, answers a credential-protected endpoint verified against a rotating key set at a scripted UTC, forwards calls to a callback owned by a third process, and publishes all of it through a directory, sometimes late. A third process adopts recruited interfaces and owns callbacks; a legacy peer speaks protocol version 1 only. Three client lanes mix every delivery mode, stream shape, balancing permission and credential kind, call stored interfaces of ended boots, burst past the in-flight budget and open raw sessions that send garbage, under swarm network chaos without bit flips (corruption is the foundations campaign's explicit experiment, never arbitrary loss on healthy TCP).

The oracles are application ledgers only: boots, publications and executions written by handlers the moment user code runs; the streams campaign's producer and consumer ledgers; the security campaign's issue/receipt ledger. A balanced call is judged against the cap its policy alone allows (the attempts the balancer reports through its observation hook only tighten that bound; they are the system's own account, not an oracle). A seed's record (its history, the ledger digest and the campaign's trace events captured from the simulation timeline) must be identical when the seed runs again, after other seeds ran in the same process, and when every poll is slowed on the host by spinning or sleeping: no decision may come from wall-clock time.

```bash
cargo xtask sim run rpc-qualification          # coverage-guided, every seed twice under the RNG canary
cargo nextest run -p moonpool-rpc-sim --test qualification
cargo run --release -p moonpool-rpc-sim --example rpc_soak      # real TCP, both Tokio flavors
```

Mutation checks keep the oracles honest: turning off the incarnation check, retransmitting single attempts, acknowledging stream items on read, skipping the queue model's release on drop, or skipping the credential check each turns the bounded campaign red.

## Reading a Runtime in Production

`RpcHandle::stats()` is a snapshot of counters and gauges; `RpcMetrics` exports them. A few to watch:

- `overload_refusals` climbing with `inflight_requests` at its budget: callers are pushed back (`Overloaded`, `NotAdmitted`) rather than queued without bound. Retrying them is safe; they never ran.
- `retained_calls` and `pending_calls` that never fall: callers holding reliable calls to a peer that is gone. `get_reply_unless_failed_for` bounds that wait by sustained failure.
- `streams_producing` with `producer_window_reserved` stuck: consumers that stopped reading; the inbound idle timeout and ping probe release them.
- `ResourceProbe::outstanding()` after a shutdown: every gauge an owner still holds. All zero means the runtime left nothing behind.

`ShutdownReport` says whether a graceful shutdown drained, how many calls it ended and how many replies it abandoned at the deadline.
