# What moonpool-rpc Promises

<!-- toc -->

The previous chapter walked through the runtime feature by feature. This one
is the contract in one place: what `moonpool-rpc` promises, what it does not,
and how we know. Every promise is backed by tests on real TCP and by
deterministic campaigns in `moonpool-rpc-sim` whose oracles never ask the
transport what it thinks happened. The full evidence record (every behaviour
row, the seeds, the mutation checks and the measurements) lives in the
repository at `docs/plans/moonpool-rpc/qualification.md`.

## The Promises

**Every failure says what it proves.** An `RpcError` carries a reason and one
of `NotAdmitted`, `MaybeExecuted` or `Executed`. `NotAdmitted` means no
handler saw that attempt, ever. A refused retransmission of a reliable call
says `MaybeExecuted`, because an earlier copy may have run.

**One attempt is one attempt.** `try_get_reply`, `attempt` and `send` never
retransmit: at most one execution per call. `get_reply` sends again on every
new connection, so it may execute more than once, and the campaigns require
that duplicate to happen.

**A reference names one incarnation, forever.** A process restarted at the
same `ip:port` refuses its predecessor's references with `StaleIncarnation`,
before any handler runs, whatever carried the reference: a direct call, a
reliable call retained across the restart, a stream, a balanced set, a stored
blob or a callback forwarded through a third process. Learning the new
incarnation is an explicit application step.

**Streams are ordered, bounded and end exactly once.** Items arrive in order
without gaps or repeats. A producer never runs further ahead of what the
consumer's application took than its window. Every item that arrived is
observable before the one terminal outcome. An abandoned stream releases its
producer.

**Balancing obeys its permissions.** A balanced call runs at most as often as
its retry and duplicate permissions allow, only on incarnations of the set it
was given. Every attempt reserves and releases the queue model exactly once,
late losers included.

**Nobody unauthorized runs.** Private endpoints fail closed. A credential is
verified before dispatch on every route, local calls included, and refused
for what is actually wrong with it. Key sets rotate by replacement. Validity
time comes from an injected UTC clock, never from the scheduling clock.

**Versions interoperate or refuse observably.** Adjacent protocol versions
talk. An unsupported one is refused at the handshake, never admitted.

**Shutdown is honest.** `RpcHandle::shutdown(grace)` closes admission, drains
within the grace on provider time, and ends what is left with the execution
knowledge it has.

**Nothing outlives its owner.** Dropping a driver releases its tasks,
connections and state. Every byte, window and owed reply is held by an owner
and returned exactly once. `ResourceProbe::is_at_baseline` checks all of it
after the runtime is gone.

**Service comes back.** After faults stop, the qualification campaign must
regain every service within a declared bound of simulated time: 30 s for
fresh publications, calls, a new stream, a balanced call, a credentialed
call, the legacy peer, recruitment and callbacks.

## What It Does Not Promise

These are deliberate:

- **No exactly-once delivery, no durable queue, no duplicate suppression.**
  Ambiguity is reported, never hidden. An operation that must not run twice
  needs an application key.
- **No stream resumption.** A broken stream ends. A new one is the
  application's decision.
- **No transparent incarnation or endpoint refresh.** A stale reference
  fails. Learning the new one is explicit.
- **No FoundationDB wire compatibility.** The frame, envelope and codec are
  this crate's own.
- **No confidentiality or peer authentication in plaintext mode.**
  `SecurityConfig::trusted_network()` claims nothing. A credential goes over
  plaintext only where each side opts in (`send_credentials_over_plaintext`
  and `accept_credentials_over_plaintext`); otherwise it is withheld
  (`CredentialWithheld`, never sent).
- **No multi-tenant isolation.** Budgets bound resources per connection and
  runtime. They are not a tenant scheduler.
- **No platform beyond Moonpool's own.** Linux and macOS on Tokio
  (current-thread and multi-thread). On `wasm32-unknown-unknown`, the
  protocol and the simulation build without the native TLS and JWT adapters.
- **No performance envelope.** The measured numbers are calibration points on
  recorded hardware, not service-level objectives.

## How We Know: Seven Campaigns

Each campaign in `moonpool-rpc-sim` is a `SimulationBuilder` with process
groups, a surviving workload, swarm network chaos (`Chaos::Network(Swarm)`
plus `Chaos::BuggifyKnobs`, which pushes every `PeerPolicy` knob to an
extreme on some seeds) and a scripted `FaultInjector`. The oracle is always
**outside the transport**: handlers write a ledger the moment user code runs,
and the workload judges its own outcomes against it at the end.

| Campaign | What it drives | What the ledger must show |
|---|---|---|
| `rpc-foundations` | a server, a relay that calls the server, raw malformed sessions, forged references, bit flips and a corrupting wire | at most one receipt per request id, one for every reply, none for `NotAdmitted` |
| `rpc-delivery` | every delivery mode, lost replies, broken promises, crashes mid-handler, simultaneous dials, bootstrap names | a single attempt never shows two executions; reliable delivery must sometimes show two |
| `rpc-interfaces` | three participants restarted at their own address, directories, recruitment, forwarding to a third process, an impostor interface | every execution names its own boot and instance; a stale refusal names a boot that really ended |
| `rpc-balance` | three servers with a character per boot, every permission, hedges, cancels, outages of every alternative | no job runs more often than its permissions allow; every attempt reserved and released once |
| `rpc-streams` | eager, slow, abandoned and stalled consumers, saturation, producer crashes and graceful shutdowns | each consumed item is the produced one at its place; credit is earned only when an item is taken |
| `rpc-security` | real JWTs against a rotating key set at a scripted UTC, a version 1 peer, every credential kind | no unauthorized execution, ever; every refusal names what is wrong |
| `rpc-qualification` | all of the above at once, five processes, three client lanes, a recovery phase and resource baselines | the other campaigns' ledgers, plus service back within the bound and every runtime at baseline |

Mutation checks keep the oracles honest. Each mutation was applied to the
runtime alone and the bounded qualification suite run on seeds 1 to 30:

| Mutation | Seeds red |
|---|---|
| Incarnation check disabled at admission | 26 / 30 |
| Single attempts retained and retransmitted | 6 / 30 |
| Stream items acknowledged on arrival instead of on take | 21 / 30 |
| Queue-model reservation not released on drop | 7 / 30 |
| Credential check skipped | 30 / 30 |

Only the foundations campaign draws the simulator's bit flips. Corruption is
one explicit experiment, whose contract is that a checksum catches it and
closes the session. Every other campaign runs
`NetworkFaultMask::all().without(NetworkFault::BitFlip)` on partitions,
clogs, random closes, connect failures, black holes, latency and clock drift,
so none of them mistakes corruption for message loss on healthy TCP.

To run them:

```bash
cargo xtask sim run rpc-qualification          # coverage-guided, every seed twice under the RNG canary
cargo xtask sim run rpc-streams                # or any other campaign name
cargo nextest run -p moonpool-rpc-sim --test qualification
cargo run --release -p moonpool-rpc-sim --example rpc_soak -- --smoke   # real TCP, both Tokio flavors
```

The campaigns found real bugs on their first runs. The balance campaign found
a lockout: a client whose alternatives all looked failed kept failing every
call after the servers recovered, because nothing ever dialed them again. The
streams campaign found a leak: a consumer that gave up during a partition
left its producer waiting on credit forever. The interfaces campaign found a
simulator bug: an in-place restart could poll the new boot before the
executor had dropped the old one. A seed's record (its history, the ledger
digest and the captured trace) must be identical when the seed runs again,
after other seeds ran in the same process, and with every poll slowed on the
host, so no decision may come from wall-clock time.

## Reading a Runtime in Production

`RpcHandle::stats()` returns an `RpcStats` snapshot. `RpcMetrics` exports it.
A few fields to watch:

- `overload_refusals` climbing with `inflight_requests` at its budget: callers
  are pushed back (`Overloaded`, `NotAdmitted`) instead of queued without
  bound. Retrying them is safe. They never ran.
- `retained_calls` and `pending_calls` that never fall: callers holding
  reliable calls to a peer that is gone. `get_reply_unless_failed_for` bounds
  that wait by sustained failure.
- `streams_producing` with `producer_window_reserved` stuck: consumers that
  stopped reading. The inbound idle timeout and its ping probe release them.
- `injected_*` counters: the runtime's own buggify points (see the next
  chapter). Always zero outside a simulation.
- `ResourceProbe::outstanding()` after a shutdown: every gauge an owner still
  holds. All zero means the runtime left nothing behind.

`ShutdownReport` says whether a graceful shutdown drained, how many calls it
ended and how many replies it abandoned at the deadline.
