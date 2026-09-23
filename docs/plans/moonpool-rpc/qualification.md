# moonpool-rpc qualification (#219)

Release-readiness record for the agreed moonpool-rpc scope: every behaviour row of [parity](parity.md) with its owned evidence and the combined qualification evidence, the mandatory scenarios, the commands and seeds that reproduce them, the mutation checks, the measurements with their hardware and toolchain, the residual risks and the explicit non-claims. Book chapter: `book/src/part4-networking/03-rpc-guarantees.md`.

## Integration base

- Base: `2edfbc0` on `claude/epic-albattani-v7vs90`, with #213–#218 merged (#216 streams/credit/admission, #217 balancing, #218 security, TLS/JWT, protocol v2, graceful shutdown, observability and the facade `rpc` feature).
- FoundationDB behavioural reference: `c0c44752df676e4a2d532b5cdb4bf96728a30b78` (`fdbrpc/FlowTransport.cpp`, `fdbrpc/include/fdbrpc/fdbrpc.h`, `LoadBalance.actor.h`, `sim2.cpp`, `flow/Net2.cpp`).
- #219 commits, in order: the bit-flip mask for non-corruption campaigns; `ResourceProbe::outstanding` / `is_at_baseline`; seeded malformed-input property tests; the `sim-rpc-qualification` campaign; the real-network soak example; `TCP_NODELAY` on Tokio connections; CI; the soak latency fix; these documents.

## What the package added

| Change | Why |
|---|---|
| `crates/moonpool-rpc-sim/src/qualification/` (`sim-rpc-qualification`, `tests/qualification.rs`) | The combined campaign: every contract of #213–#218 at once, with a declared recovery phase, resource baselines, semantic replay across runs and host speeds. |
| `ResourceProbe::outstanding()`, `Outstanding`, `ResourceProbe::is_at_baseline()` (moonpool-rpc) | `is_released` proved only tasks, connections and the runtime state were gone. A baseline also needs the gauges owners hold: owed replies, produced streams, queued bytes, reserved consumer and producer windows, buffered stream bytes. Each is RAII-held and returned once, so the probe reads them after the driver is gone. `every_gauge_returns_to_zero_once_its_owners_are_gone` (+ flavors). |
| `crate::without_corruption()` masks `NetworkFault::BitFlip` in delivery, interfaces, balance, streams, security and qualification | The builder's mask defaults to every family, so Swarm could draw in-flight bit flips in campaigns that do not judge corruption. Corruption stays one explicit experiment: the foundations campaign (its corrupting wire plus `network_bit_flips_are_caught_by_frame_checksums`). No campaign relied on a bit flip for a required gate; every bounded suite stays green with the mask. |
| `crates/moonpool-rpc/tests/malformed.rs`, `security::jwt::tests::mutated_tokens_never_panic_nor_verify_as_someone_else` | Seeded, dependency-free property tests over every untrusted decoder, with a counting allocator bounding the largest allocation. |
| `TokioNetworkProvider` sets `TCP_NODELAY` (best-effort) on connect and accept (moonpool-core) | Nothing set it; FDB does (`flow/Net2.cpp` `Connection::init`, `FLOW_TCP_NODELAY` default 1; `SSLConnection::init`). Not RPC-specific (hyper/tonic use the same streams); the simulator has no Nagle model, so production timing now matches it. `connected_and_accepted_streams_disable_nagle`. |
| `crates/moonpool-rpc-sim/examples/rpc_soak.rs` | Real-network soak and performance on both Tokio flavors; `--smoke` for CI. |
| CI: `rpc-qualification` in the sim matrix, `rpc-soak` job, macOS `cargo nextest run -p moonpool-rpc` + soak smoke | Qualification gates in CI; the macOS half of the platform row. |

## The qualification campaign

Five processes and three client lanes (`crates/moonpool-rpc-sim/src/qualification/mod.rs` documents the roles):

- **server** ×3, two datacenters by address, one verifying runtime per boot at the same `ip:port`: a base `Work` instance, a fresh instance per recruited configuration, the streams campaign's `ScanItems` producer (unchanged, writing its ledger), the security campaign's `PrivateEcho` behind the production JWT/JWKS verifier over a rotating key set at a scripted UTC, a local caller through the same admission check, well-known directory/recruiter/forwarder, registrations sometimes delayed after a boot (BUGGIFY), graceful shutdown draining within a drawn grace.
- **third**: adopts recruited members arriving inside a request and calls them through its own runtime; owns a callback endpoint (fresh per boot) servers call when a request hands them its reference.
- **legacy**: protocol version 1 only.
- **workload lanes** ×3 (lane 0 leads): a runtime speaking versions 1–2, a version 1 only runtime, a load balancer. The mix: at-most-once calls (some with deadlines shorter than the work, some abandoned by their caller), reliable calls held across requested cuts (some beside a stream on the same session), calls bounded by sustained failure, one-way sends, streams (eager with finish/fail/drop ends, slow with a two-item window, abandoned before/after the first item, endless), balanced calls (at most once, retry after ambiguity, hedged, cancelled), credentialed calls (valid, short-lived presented after expiry, from a retired key, anonymous, forged, wrong audience), stored interfaces of ended boots (at-most-once and reliable), directory lookups, recruitment through the third participant, callbacks (current and stale), legacy calls, version 1 calls, overload bursts, a raw session sending a 4 GiB frame header, and graceful shutdowns requested under held calls and an endless stream. The lead lane probes every server that starts draining.
- **faults**: Swarm network chaos without bit flips, `Chaos::BuggifyKnobs` (every peer-policy, budget and window knob), and a script over the 20 s chaos window: crash + hold-down, in-place restart and graceful reboot of servers, third and legacy restarts, requested cuts, isolation of a lane from every server, UTC steps, jumps, loss and restore, key rotations. The reboot-storm variant restarts one server in place at least ten times.

**Oracles (application data only).** The qualification ledger (boots bumped before touching the network, publications written by the publisher before advertising, executions written when user code gets the request), the streams producer/consumer ledger, the security issue/receipt ledger, the balancer's observation hook. Handlers assert on the spot: "rpc qual a request ran only in the instance it named", "rpc qual no handler of an ended boot ran", "no request reaches a handler without a credential its endpoint accepts". After late losers drained and the network settled (3 s), every call is judged again: "rpc qual every execution was in the instance its call named", "rpc qual a replied call executed", "rpc qual a not-admitted call never executed", "rpc qual a single attempt never executed twice", "rpc qual a balanced job ran at most as often as permitted", "rpc qual a balanced job ran only in an incarnation of its set", "rpc qual a stream ran only in the incarnation it named", "rpc qual each consumed item is the produced one at its place", the streams campaign's end-of-stream rules and the security campaign's receipt rules. "rpc qual a learned interface came from the instance it names" is checked at every lookup and recruitment. "rpc qual no credential appeared in a trace field" scans the captured trace (campaign events, `rpc_request_denied` audit events, shutdown events) for every minted token's signature.

**Recovery and baselines.** After the script stops, the lead lane must regain, within `RECOVERY_BOUND` = 30 s of simulated time, for every server a fresh publication, a unary call, a new stream (never a resumed one) and a credentialed call, plus a balanced call over the fresh set, a legacy call, recruitment through the third participant and a callback: "rpc qual service recovered within the declared bound". The bound is the longest reconnect backoff the knobs allow (3 s), a session's ping timeout (up to 6 s), the balancer's exclusion (up to 2 s) and all-alternatives probe, a lookup and a stream per server, with margin; observed over seeds 1–30: 0.23–5.4 s. Then every stream a current boot produced must end within `RELEASE_BOUND` = 30 s (the inbound idle timeout is capped at max(8 s, 4 × ping interval)), each lane's client runtime and every serving runtime must be idle, and the queue model must have released each reservation exactly once. Every boot checks that its process's earlier runtimes are at baseline ("rpc qual a restarted process's old runtime is at baseline"); each lane checks its own runtimes after their drivers drop; the end of the run checks every ended boot's probe ("rpc qual every runtime of the run returned to baseline").

**Replay.** A run's record is the lanes' histories, the ledger digest and the captured trace (sim time, source and fields of every campaign, audit and shutdown event). `same_seed_replays_the_same_semantic_history` (seeds 3, 11, 27), `repeated_complete_runs_are_isolated` (seeds 5, 9, 5, 13, 5 as separate complete simulations in one process: seed 5's record identical each time, every run at baseline), `host_speed_does_not_change_semantics` (seed 7 plain, with a 2,000-iteration spin on every poll, and with the thread asleep 200 µs every 64th poll of every process and lane: identical records), `reboot_storm_returns_to_baseline` (seeds 1–3: ten-plus in-place restarts of one server, every boot finds the previous runtimes at baseline, service recovers).

### Reproduction

```bash
cargo nextest run -p moonpool-rpc-sim --test qualification        # bounded suite
QUAL_SEED_OFFSET=30 cargo test -p moonpool-rpc-sim --test qualification bounded   # another 30-seed window
cargo xtask sim run rpc-qualification                             # coverage-guided, RNG canary
```

One failing seed: `qualification_campaign(QualificationConfig::campaign(), &records).set_debug_seeds(vec![seed]).set_iterations(1).run()` (see `.claude/skills/debug-a-seed`).

### Results

- `bounded_campaign_hits_every_required_scenario`: seeds 1–30, each twice under `check_determinism`, 50 required scenarios, no violation, every run recovered and at baseline; about 55–65 s in the debug profile on 4 cores (nextest slow-timeout override for this binary rather than a smaller budget). The same test over seeds 31–60 and 61–90 also fired every required scenario with no violation (90 seeds, 180 runs). The rarest required scenarios fired on 3–5 seeds of 30.
- `cargo xtask sim run rpc-qualification` (sancov-instrumented, `until_coverage_stable(10, 1000)`, every seed twice under the canary): 124 iterations, 0 failed, 81 of 81 sometimes assertions hit, code coverage stable at 28,236 of 70,459 edges for 10 seeds.
- The other campaigns re-run with `cargo xtask sim run` after the bit-flip mask (coverage-guided, every seed twice under the canary), all clean: `rpc-foundations` 69 seeds, 23/23 sometimes, 10,806 edges; `rpc-delivery` 36 seeds, 47/47, 11,530 edges; `rpc-interfaces` 71 seeds, 30/30, 11,908 edges; `rpc-balance` 107 seeds, 30/30, 11,370 edges; `rpc-streams` 134 seeds, 37/37, 13,015 edges; `rpc-security` 48 seeds, 46/46, 15,260 edges.

The campaign's first bounded runs caught two wrong oracles, not transport bugs: a reliable call's retransmission refused by a draining server honestly reports `MaybeExecuted` (an earlier copy may have run), now its own scenario; and assertion messages of 64 bytes or more are truncated in the assertion table, so required-scenario names were shortened below that limit.

### Mutation checks

Each mutation applied to `crates/moonpool-rpc/src` alone, the bounded test run on seeds 1–30 under the canary, the source restored (not committed):

| Mutation | Seeds red | Caught by |
|---|---:|---|
| Incarnation check disabled at admission (`transport::Shared::admit`) | 26 / 30 | "rpc qual a request ran only in the instance it named" (1,142), "rpc qual a reply came from the instance its reference named", "rpc qual a stream ran only in the incarnation it named", "rpc qual a balanced job ran only in an incarnation of its set", "rpc qual a stored interface of an ended boot never answers" |
| Single attempts retained and retransmitted (`start_call` forces `Delivery::Reliable`) | 6 / 30 | "rpc qual a single attempt never executed twice" (36), "a credential refusal is never admitted" |
| Stream items acknowledged on arrival instead of on take (`stream::consumer`) | 21 / 30 | "a producer never runs ahead of consumption beyond its window" (692) |
| Queue-model reservation not released on drop (`balance::model::Reservation`) | 7 / 30 | "rpc qual every late loser ended within its bounds", "rpc qual the queue model released each reservation once" |
| Credential check skipped (`authorize` bypassed) | 30 / 30 | "no request reaches a handler without a credential its endpoint accepts" (5,670), "a reply carries the principal its own request's credential proved" |

## Row status

Every row of parity.md, its owned evidence (the P1–P6 sections) and what the qualification adds. **Proven** means passing unit, real-network and deterministic evidence appropriate to the row; **excluded** means an explicitly approved exclusion.

| Row | Owned evidence | Combined (#219) | Status |
|---|---|---|---|
| Dynamic allocation and registry | P1 | Every boot re-registers the same slots; handler-side instance check; incarnation mutation 26/30 red | Proven |
| Well-known bootstrap | P2 | Directory, recruiter, forwarder and adopter are well-known across every reboot; lookups before registration refused ("lookup reached a boot before it registered") | Proven |
| Destruction and in-flight admission | P3 | Graceful shutdown refuses new admissions while draining ("shutdown refused new admissions") | Proven |
| ABA and registry reuse | P3 | Same-address reboots with slot reuse; stale I1 never reaches I2 on any carrier | Proven |
| Adjusted endpoint groups | P3 | Not recombined (the role interface is exercised by the interfaces campaign) | Proven |
| Typed interfaces | P1 | Streaming/unary/private endpoints on one runtime | Proven |
| Interface serialization/storage/forwarding | P3 | Recruited members forwarded to the third participant; stored publications of ended boots refused | Proven |
| Local delivery | P1 | Local caller through the same credential check ("local call denied by the same policy") | Proven |
| Remote delivery and peer-relative replies | P1 | Every reply checked against the instance its reference named | Proven |
| General callbacks | P3 (real TCP) | Now in simulation: "callback invoked through a third participant", "stale callback refused" (closes the P3 residual) | Proven |
| Public/private endpoints and endpoint compatibility | P6 | Private endpoint under key rotation and UTC transitions; auth mutation 30/30 red | Proven |
| Framing and incremental parsing | P1 | `malformed.rs::random_frames_never_panic_or_overallocate` | Proven |
| Size and integrity limits | P1 | Malformed property tests (largest allocation bounded); raw 4 GiB header closes only its session ("malformed peer closed, other sessions unaffected") | Proven |
| Unsent bytes versus reliable retention | P2 | Requested cuts under held reliable calls: "reliable request executed more than once", also beside a stream | Proven |
| Buffering and batching | P4 (real TCP; no sim bandwidth model) | Soak stream table; `stream_saturation` re-measured in release (below) | Proven |
| Inbound/outbound and handshake | P1, P6 | v1 runtime refused by verifying servers at the handshake, `NotAdmitted` | Proven |
| Simultaneous connections | P2 | Sharing mode drawn per process by knob; not a dedicated combined scenario | Proven |
| Reconnect/backoff/jitter | P2 | Recovery within the declared bound after every fault | Proven |
| Peer references and outstanding replies | P2 | Idle baselines of client and serving runtimes after recovery | Proven |
| Idle connections and keepalive | P2, P4 | Streams released within `RELEASE_BOUND` | Proven |
| Connection cleanup | P2 | Baseline after every boot and at the end, repeated runs | Proven |
| send | P2 | One-way ops judged by the single-attempt rule | Proven |
| tryGetReply | P1 | Ledger oracle; retained-single-attempt mutation 6/30 red | Proven |
| getReply | P2 | Duplicates required; retained call to I1 ends terminally | Proven |
| getReplyUnlessFailedFor | P2 | "reliable call bounded by sustained failure" | Proven |
| getReplyStream | P4 | Streams against rebooting, draining producers; never resumed | Proven |
| Reply promise and late completion | P2 | Balanced late losers after the call returned | Proven |
| Broken promise versus explicit no-reply | P2 | Streams ending in a dropped producer judged "a broken promise follows every item" | Proven |
| Cancellation and timeout ambiguity | P2 | "timeout after execution", "caller dropped a call in flight", "reply lost after execution reported ambiguous", "reply won a race with its server going down" | Proven |
| Endpoint-not-found and permission errors | P2, P6 | Stale and denied calls never executed | Proven |
| Address failure versus disconnect | P2 | Not recombined beyond sustained-failure calls | Proven |
| Endpoint/permanent/unauthorized state | P2, P6 | Denials never remembered as endpoint failure (recovery calls succeed after rotations) | Proven |
| Sustained failure and recovery | P2 | Sustained-failure calls under cuts; recovery | Proven |
| Well-known recovery | P2 | Directories answer every new boot | Proven |
| Retry/DNS helpers | P2 | Not recombined (no DNS names in the qualification topology) | Proven |
| Sequence and ordered stream delivery | P4 | "each consumed item is the produced one at its place" | Proven |
| ACK endpoint and cumulative accounting | P4 | ACK-on-read mutation 21/30 red | Proven |
| Producer readiness and byte limits | P4 | "slow consumer held the producer to its window", "producer exhausted credit and resumed" | Proven |
| Consumer cancellation/abandonment | P4 | Abandoned before/after first item; no orphan beyond `RELEASE_BOUND` | Proven |
| Normal completion/errors/disconnect | P4 | "stream ended by a disconnect mid-stream", "stream failed by a graceful shutdown" | Proven |
| Alternatives and locality | P5 | Two datacenters by address | Proven |
| Queue and latency model | P5 | Release-on-drop mutation 7/30 red | Proven |
| Penalties/exclusion/temporarily-behind backoff | P5 | Exclusion under swarm faults; recovery balanced call | Proven |
| Retry permission | P5 | Permission oracle re-checked after late losers | Proven |
| Second requests/hedge timing/budget | P5 | "balanced call hedged", "hedge budget exhausted" | Proven |
| Late losers and model updates | P5 | "late loser completed after its call returned" | Proven |
| Stale alternative sets | P5 | "stale incarnation skipped for a healthy one", stale sets replaced explicitly | Proven |
| Generic duplication/comparison/busy-score hooks | P5 | Not recombined (observation hook only) | Proven |
| TLS and peer trust | P6 (real TCP only; the simulation runs no TLS) | — | Proven |
| Allowed addresses and private/public access | P6 | — | Proven |
| Authorization and key/JWKS rotation | P6 | "expired credential denied before dispatch", "rotated key denied then new key accepted"; mutated-token property test | Proven |
| Protocol RNG versus crypto entropy | P6 | Replay equality with EdDSA tokens (deterministic signatures) | Proven |
| Logical time versus UTC | P6 | Host-speed replay equality with scripted UTC | Proven |
| Transport/schema evolution and stored interfaces | P6 | Legacy v1 peer interoperates; v1 client refused; mutated golden envelopes and references | Proven |
| mTLS deferral | P6 | Non-claims audited in the book and these documents | Excluded (user decision) |
| Task lifetime and structured cancellation | P1 | `is_at_baseline` after every process death | Proven |
| Ordering/fairness/control progress | P4 | Overload bursts beside streams | Proven |
| Admission and resource limits | P4 | "request refused overloaded before admission", "service recovered after an overload burst"; soak overload table | Proven |
| Observability and metrics | P6 | Redaction invariant over the captured trace; "a denial was audited in the trace" | Proven |
| Graceful/abrupt shutdown | P6 | "graceful shutdown drained an in-flight call", "graceful shutdown ended work at its deadline", "shutdown refused new admissions" | Proven |
| Faults and deterministic replay | #219 | The campaign, its canary, semantic replay, the explicit corruption suite (foundations) | Proven |
| Process/run isolation and post-fault recovery | #219 | Baselines per boot and per run, repeated complete runs, reboot storm, declared recovery bound | Proven |
| Platform/performance/release | #219 | Linux: every suite, both Tokio flavors, soak; wasm32: protocol and simulation builds (CI); macOS: CI job added, **not run here** | Proven on Linux; macOS pending its first CI run |

## Mandatory scenarios

| Required scenario | Combined evidence (required in the bounded test) |
|---|---|
| Lost reply after server execution; no unintended at-most-once retransmission; allowed reliable duplicates | "reply lost after execution reported ambiguous", "a single attempt never executed twice" (always), "reliable request executed more than once" |
| Reply/disconnect races, timeout ambiguity, caller drop, late replies, broken promise/no-reply | "reply won a race with its server going down", "timeout after execution", "caller dropped a call in flight", "late loser completed after its call returned", broken-promise stream ends |
| Endpoint destruction, registry reuse, same-address reboot, stale I1 rejection, fresh I2 publication | "stale I1 refused while I2 served at the same address", "retained reliable call to I1 ended terminally", "stored interface of an ended boot refused", "participant republished after a same-address reboot", "lookup reached a boot before it registered" |
| D/E recruitment and interface transmission to a third participant | "recruited interface invoked by the third participant", "callback invoked through a third participant", "stale callback refused" |
| Slow consumption, abandoned stream before/after first item, ACK/credit exhaustion and disconnect | "slow consumer held the producer to its window", "stream abandoned before its first item", "... after its first item", "producer exhausted credit and resumed", "stream ended by a disconnect mid-stream" |
| Reconnect, monitor state transitions, DNS refresh, healthy-address/dead-endpoint distinction | "reliable call bounded by sustained failure", recovery within the bound; DNS refresh stays the delivery campaign's |
| Balancer fallback, separate retry/duplicate permission, hedge budget exhaustion and late losers | "balanced call fell back to another alternative", "balanced call hedged", "hedge budget exhausted", "late loser completed after its call returned", permission oracle |
| Task cancellation before first poll, shutdown, resource reclamation, process/run isolation | Baselines per boot, per lane and per run; "graceful shutdown drained an in-flight call"; repeated runs; reboot storm |
| Authentication denial, key/time changes, protocol/schema mismatch and actual crypto interoperability | "expired credential denied before dispatch", "rotated key denied then new key accepted", "private endpoint refused an unauthenticated call", "local call denied by the same policy", "adjacent-version peer interoperated", "unsupported version refused observably"; real crypto on real TCP (P6 `tests/tls.rs`, `security::jwt`) |
| Semantic replay, RNG canary, required hits and recovery after faults stop | Canary on every seed; record equality across replays, repeated runs and host speeds; "every service recovered after the faults" |

## Measurements

Hardware and toolchain: Linux 6.18 x86_64, 4 shared vCPUs (Intel Xeon @ 2.10 GHz), rustc 1.95.0, Tokio 1.53.1, release profile, localhost TCP, both runtimes in one process; commit `5412e72`. Calibration points, not acceptance envelopes.

**Unary, open loop** (`rpc_soak`, 5 s per row; latency from each call's start; the generator starts calls up to about 2 ms late because Tokio's timer ticks in milliseconds, reported separately):

| Runtime | Offered/s | Payload | Max in flight | Completed/s | p50 | p99 | p99.9 | max | failed |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| current-thread | 1,000 | 64 B | 256 | 989 | 32 µs | 101 µs | 497 µs | 2.5 ms | 0 |
| current-thread | 5,000 | 64 B | 256 | 4,941 | 71 µs | 190 µs | 499 µs | 1.6 ms | 0 |
| current-thread | 20,000 | 64 B | 256 | 19,803 | 217 µs | 515 µs | 1.31 ms | 2.6 ms | 0 |
| current-thread | 5,000 | 4 KiB | 256 | 4,944 | 90 µs | 244 µs | 757 µs | 2.3 ms | 0 |
| current-thread | 1,000 | 64 KiB | 256 | 987 | 78 µs | 346 µs | 985 µs | 2.8 ms | 0 |
| multi-thread (2) | 1,000 | 64 B | 256 | 989 | 75 µs | 244 µs | 1.12 ms | 2.5 ms | 0 |
| multi-thread (2) | 5,000 | 64 B | 256 | 4,939 | 87 µs | 398 µs | 2.08 ms | 5.3 ms | 0 |
| multi-thread (2) | 20,000 | 64 B | 256 | 19,770 | 206 µs | 1.04 ms | 2.83 ms | 8.0 ms | 0 |
| multi-thread (2) | 5,000 | 4 KiB | 256 | 4,939 | 118 µs | 542 µs | 1.78 ms | 7.7 ms | 0 |
| multi-thread (2) | 1,000 | 64 KiB | 256 | 989 | 148 µs | 416 µs | 3.05 ms | 4.2 ms | 0 |

Peak queued bytes stayed at or under 17 KiB, peak live heap 3.3 MiB (20,000/s). With the caller capped at 16 or 1 calls in flight, the excess is shed by the client, never queued by the runtime.

**Reply streams**, 1 KiB items, consumed eagerly (MiB/s; peak queued bytes):

| Runtime | 1 × 64 KiB | 1 × 1 MiB | 8 × 64 KiB | 8 × 1 MiB | 64 × 64 KiB | 64 × 1 MiB |
|---|---:|---:|---:|---:|---:|---:|
| current-thread | 228 | 374 | 220 | 379 (3.1 MiB) | 122 (2.6 MiB) | 146 (60 MiB) |
| multi-thread (2) | 152 | 235 | 219 | 263 (5.0 MiB) | 184 (0.4 MiB) | 165 (61 MiB) |

Queued bytes stay within the reserved windows: 64 streams × 1 MiB queue up to 64 MiB for one peer, which is what those windows grant (the per-connection and per-runtime producer window budgets bound it).

**Overload and recovery**: 8,192 calls of 200 ms each at once on one session: 1,024 admitted (the per-connection in-flight budget), 7,168 refused `Overloaded` before admission (never executed), the next call served 55 µs (current-thread) / 115 µs (multi-thread) after the burst drained, at most 3 driver tasks, peak live heap 1.8 / 2.7 MiB; every probe back at baseline after the runtimes dropped, and live heap back to zero after each flavor.

**`TCP_NODELAY` A/B** (`balance_latency` at 1,000 calls/s × 5 s over three servers, 5% of requests stall 20 ms; two runs each, before → after):

| Configuration | p50 | p99.9 |
|---|---|---|
| single target | 1.27–1.30 ms → 98–102 µs | 34–37 ms → 22 ms |
| plain round robin | 2.58–2.62 ms → 84–87 µs | 34–38 ms → 22 ms |
| balanced | 1.42–2.31 ms → 102–107 µs | 52–53 ms → 22 ms |
| balanced + hedged | 1.36–2.31 ms → 99–104 µs | 27–29 ms → 21.5 ms (p99 23 ms → 2.5 ms) |

The remaining 22 ms tail is the injected stall. This explains P5's multi-millisecond p50s ("the transport never sets `TCP_NODELAY`, a likely cause"): it was the cause. `tcp_latency` (sequential, one call outstanding) barely moves (p50 57–61 µs → 56–57 µs; 15,400–16,800 → 16,600–17,400 calls/s), as expected, since Nagle only holds a segment behind an unacknowledged one.

**`stream_saturation` re-measured in release** (P4 recorded debug numbers): one stream of 4 KiB items gives 125 / 265 / 313 / 502 / 692 MiB/s at 16 KiB / 64 KiB / 256 KiB / 1 MiB / 4 MiB windows, so in release throughput keeps growing past the 1 MiB default (P4's debug run flattened at 1 MiB). Unary p50/p99 beside eight saturating streams: batch 1: 6.7 / 182 ms; 16: 3.4 / 4.9 ms; 64 (default): 3.6 / 5.7 ms; 256: 5.0 / 9.9 ms. Push-back unchanged: 4,096 slow calls, 1,024 admitted, 3,072 refused, session kept. **Recalibration decision:** the defaults stay (1 MiB window: a per-stream memory bound, below FDB's 2 MB; batch 64 within noise of 16); an application streaming bulk data raises its window explicitly (`get_reply_stream_with_window`, `StreamPolicy::max_window_bytes` 16 MiB).

Budgets recorded for regressions (this machine, release): unary p99 at 5,000/s of 64 B under 0.5 ms (both flavors); no failed call and no runtime queueing under the soak loads; overload refused before admission with recovery within milliseconds; every runtime at baseline afterwards. CI's `--smoke` checks the invariants only.

## Audit

- **Dependency trees** (`cargo tree -e no-dev`): `moonpool-rpc` default = `moonpool-core` (no Tokio), futures, prost, tracing, xxhash-rust; `--no-default-features` drops prost; `derive` adds only `moonpool-rpc-derive`; `tls,jwt` add futures-rustls/rustls/ring and jsonwebtoken. No `moonpool-sim`, explorer or Tokio runtime in any of them (CI gate). Facade: `--no-default-features --features tokio` contains no moonpool-rpc; `tokio,rpc` adds only moonpool-rpc. Duplicates, all behind the optional `jwt` feature or proc macros: `rand` 0.8 and `rand_core` 0.6 (jsonwebtoken, rsa), `syn` 2/3 (futures-macro); upstream, not fixed here.
- **Process-scoped ownership**: no `static`, `thread_local!`, `OnceLock`, `LazyLock` or spawn in `moonpool-rpc` or `moonpool-rpc-derive` sources; every child future is driver-owned and counted by a `TaskGuard`. The one process-wide effect is the documented `jsonwebtoken` crypto provider install (feature `jwt`).
- **Clock and entropy**: the only host clock read is `SystemUtc` (an opt-in production `UtcClock`, never a default); no `Instant::now`, `thread_rng`, `OsRng`, `HashMap`/`HashSet`, `std::thread::sleep` or direct Tokio time/spawn/select in `moonpool-rpc`. TLS and signature entropy come from ring/rust_crypto. The host-speed replay test is the behavioural check.
- **Malformed input**: `crates/moonpool-rpc/tests/malformed.rs` (frames, every golden envelope at every version, references, credential sections, version ranges) and the mutated-token JWT test; in simulation, the raw 4 GiB header.
- **Diagnostics**: every `RpcError` displays its reason and execution knowledge; refusals are counted by closed-set reason and audited (`rpc_request_denied`) without credential bytes (redaction invariant); `ResourceProbe::outstanding` names whatever an owner still holds.

## Residual risks

- **macOS**: no macOS host was available; the real-network suites and the soak smoke run there only once CI runs the new macOS steps.
- The soak runs on one host over loopback; no cross-host network, no WAN latency or loss on real sockets (the simulator covers faults).
- The simulator has no bandwidth model: byte-level head-of-line figures come from real TCP only.
- The bounded qualification test takes about a minute in the debug profile (nextest override: 30 s period, 8 periods); its rarest required scenarios fire on 3–5 of 30 seeds, checked on three windows.
- The redaction invariant scans the captured trace (campaign, audit and shutdown events), not every `tracing` event of every crate.
- Previously recorded residuals stand: a vanished caller's streams are released only after the producer's inbound idle timeout and a ping timeout; a client sends a bearer credential over any session it opens (run credential-carrying runtimes over TLS or a trusted network); a version 1 caller refused by a version 2 server sees `EndpointNotFound`.

## Non-claims

The release documentation claims none of the following: exactly-once delivery, a durable reliable queue or duplicate suppression; stream resumption; transparent incarnation or endpoint refresh; mutual TLS (deferred by user decision); FoundationDB wire compatibility; confidentiality or peer authentication in plaintext mode; internet-scale multi-tenant isolation; platforms beyond Moonpool's Linux and macOS on Tokio (wasm: protocol and simulation only); performance envelopes beyond the calibration points above.
