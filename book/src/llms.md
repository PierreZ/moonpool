# Guide for AI Agents

<!-- toc -->

This is the end-to-end implementation guide for a coding agent adding
deterministic simulation to a system with Moonpool. It is deliberately
gradual: make one layer trustworthy before adding the next. The specialist
chapters explain each mechanism in depth; this page explains how to combine
them without losing determinism, correctness, or useful search guidance.

If this page disagrees with the current Rust API, inspect the source and update
the page with the code change. The most important API anchors are
`SimulationBuilder`, `Process`, `Workload`, `SimContext`, `Invariant`, the
assertion macros, and the Buggify macros.

## The Target Architecture

Keep the system, driver, oracle, and search policy separate:

```text
production logic generic over Providers
                |
       Process factory (system under test)
                |
 simulated network / time / task / random / storage
                |
  Workload factory (operations + reference model)
                |
 tracing facts ──────────────> Invariant (global safety)
 assertion macros ───────────> coverage + exploration guidance
 Buggify / Chaos ─────────────> dangerous but valid situations
                |
      SimulationBuilder (seeds, budget, replay, report)
```

The `Process` is the server code being tested. It is killed and recreated on a
reboot. The `Workload` is the test driver. It issues operations, remembers
expected outcomes, and survives process reboots within one timeline. An
`Invariant` observes facts from all actors after every simulation step. The
builder creates many fresh timelines with different deterministic seeds.

Do not put all four responsibilities in one workload. Doing so bypasses the
reboot model and makes bugs much harder to localize.

## Stage 0: Map the System Before Writing Code

First write down the following map. An agent should not start implementing the
harness until every row has an answer.

| Question | Moonpool representation |
|---|---|
| What is restarted when a server crashes? | A `Process` factory |
| What persists across a reboot? | Simulated storage, never process fields |
| What drives client operations? | A factory-created `Workload` |
| What is the independent expected result? | Workload reference model |
| What must never be transiently false? | Tracing fact plus `Invariant` |
| What local condition must never fail? | `assert_always!` or an always numeric macro |
| Which rare situations prove the test is effective? | Sometimes/reachability guidance |
| Which high-level dangerous choices are too rare naturally? | A Buggify point |
| Which operation families can suppress one another? | Operation swarm IDs |
| Which nondeterministic APIs does production code call? | Provider boundaries |

Prefer a small abstract model. For a key-value service, use a `BTreeMap` of
expected keys rather than copying the server implementation. For consensus,
track proposed and chosen values rather than reproducing Paxos inside the
driver. A reference model that shares the implementation's algorithm can share
the same bug.

Before proceeding, identify how ambiguous results are reconciled. A timed-out
write might have committed. The workload must query the system, use idempotency
keys, or model both legal outcomes; it must not blindly assume that an error
means no state change.

## Stage 1: Establish the Deterministic Boundary

All behavior that can affect a result must use Moonpool providers.

Keep the production dependency lean (`moonpool` with production provider
features, without `sim`) and put `moonpool-sim` in a separate harness package
or simulation-only target. This keeps the deterministic engine and explorer
out of production artifacts. Follow the repository's simulation-binary layout
so `cargo xtask sim list` discovers the harness.

A standalone simulation binary needs no Tokio runtime:

```rust,ignore
fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);
    let report = build_simulation().run();
    report.eprint();
}
```

| Do not use in simulated code | Use instead |
|---|---|
| `tokio::spawn` | `ctx.task().spawn_task(...)` |
| `tokio::time::sleep` | `ctx.time().sleep(...)` |
| `tokio::time::timeout` | `ctx.time().timeout(...)` |
| `tokio::select!` | `moonpool_sim::select!` |
| `std::time::Instant` for behavior | `ctx.time()` and simulated time |
| `thread_rng`, OS randomness, random UUIDs | `ctx.random()` or `sim_random_*` |
| real sockets | `ctx.network()` |
| real files for process state | `ctx.storage()` |
| background OS threads | tasks spawned through the task provider |

The executor uses one OS thread but futures must still be `Send + 'static`.
Use `Arc<RwLock<_>>`, atomics, or another `Send` structure when state is shared
between tasks. Do not use `LocalSet`, `spawn_local`, or direct Tokio runtime
construction.

Behavior-affecting iteration over an unordered collection is also a source of
nondeterminism. Prefer `BTreeMap`/`BTreeSet`, sort keys before choosing, or make
the choice through the seeded random provider. Logging order alone is less
important; protocol decisions are not.

External services, real clocks, environment changes, and global mutable state
are outside recipe replay. Move them behind providers or keep them outside the
simulated decision path.

Exit criterion: the same seed must produce the same observable trace twice.

## Stage 2: Put the System Under Test in `Process`

A process factory creates a fresh in-memory instance on first boot and every
reboot. Only simulated storage survives.

```rust,ignore
use async_trait::async_trait;
use moonpool_sim::{
    NetworkProvider, Process, SimContext, SimulationResult, TcpListenerTrait,
};

#[derive(Default)]
struct Node;

#[async_trait]
impl Process for Node {
    fn name(&self) -> &str {
        "node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        loop {
            moonpool_sim::select! {
                biased;
                accepted = listener.accept() => {
                    let (stream, peer) = accepted?;
                    // Spawn connection handling through ctx.task().
                    // Pass ctx.providers().clone() into generic production code.
                    // The handler should serve at least one healthy request.
                    let _ = (stream, peer);
                }
                () = ctx.shutdown().cancelled() => return Ok(()),
            }
        }
    }
}
```

Use `ctx.my_ip()` to bind the node. Read role tags and locality through
`ctx.topology()`. Handle `ctx.shutdown()` for graceful reboot, but do not rely
on it for crash reboot: a crash cancels the task immediately. Reopen listeners,
connections, and files on every boot.

Keep the production algorithm generic over `P: Providers` when possible. The
`Process` should be a thin simulation entry point that supplies
`ctx.providers()` to the same logic used with production providers.

A `Process::run` error ends that process and is logged, but is not by itself a
client-test failure. The workload or an invariant must observe and validate the
effect that the process exit has on the protocol.

Exit criterion: one node can boot, serve a healthy request, and stop without
leaking a task or pending event.

## Stage 3: Build a Finite, Stateful `Workload`

Workloads have three phases:

1. `setup`: setup tasks may run concurrently, followed by a barrier before
   any workload `run` method starts.
2. `run`: all workload instances execute concurrently. The first workload to
   finish triggers the shared shutdown token.
3. `check`: check tasks run after processes have been aborted and the timeline
   has settled.

The workload should be finite. Background server loops belong in `Process` and
end through shutdown. A useful workload owns a reference model and chooses from
a small, explicit operation alphabet.

```rust,ignore
use std::collections::BTreeMap;

use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationError, SimulationResult, Workload};

struct ClientWorkload {
    expected: BTreeMap<String, Vec<u8>>,
    operations: usize,
}

impl ClientWorkload {
    fn new(operations: usize) -> Self {
        Self {
            expected: BTreeMap::new(),
            operations,
        }
    }
}

#[async_trait]
impl Workload for ClientWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        if ctx.topology().all_process_ips().is_empty() {
            return Err(SimulationError::InvalidState("no server processes".into()));
        }
        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for _ in 0..self.operations {
            // Choose an enabled operation with ctx.random(), execute it through
            // ctx.network(), and update `expected` only after reconciling
            // success, failure, and ambiguous timeout outcomes.
        }
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Validate the retained model and captured timeline. Processes have
        // already stopped, so do not attempt a live RPC here.
        let _ = ctx;
        Ok(())
    }
}
```

Choose registration by lifecycle, not convenience:

| Builder method | Lifecycle | Use |
|---|---|---|
| `.workload(instance)` | Reuses that value across root iterations | Only a deliberate stateful non-exploration test |
| `.workload_factory(|| Box::new(...))` | One fresh driver per timeline | Default for one client and required for exploration |
| `.workloads(count, |i| Box::new(...))` | Fresh indexed drivers per timeline | Concurrent clients and required for exploration |

Prefer a factory from the beginning. It prevents one seed's driver state from
contaminating another and keeps later exploration/replay migration simple.

For multiple clients, use `ctx.client_id()` and `ctx.client_count()` to divide
work without overlap. Names should be useful in reports. Use
`ctx.topology().all_process_ips()` or tag queries instead of hard-coding IPs.
Coordinate their termination: because the first completed workload triggers
global shutdown, one short driver can cancel longer peers. A single workload
that spawns coordinated client tasks is often simpler when all clients must
finish together.

Perform any final live query during `run`, before returning. Use `check` for
the retained reference model and `ctx.observability()` timeline. A check error
or panic is currently logged rather than reliably becoming a workload result,
so record check contracts with Moonpool always assertions as well.

Exit criterion: the happy-path reference model agrees with the system for one
fixed seed.

## Stage 4: Start with a Reproducible Smoke Runner

Begin with one known seed and one iteration. Do not add every fault surface at
once.

```rust,ignore
use moonpool_sim::SimulationBuilder;

let report = SimulationBuilder::new()
    .processes(3, || Box::new(Node))
    .workload_factory(|| Box::new(ClientWorkload::new(20)))
    .set_debug_seeds(vec![1])
    .set_iterations(1)
    .run();

report.eprint();
assert_eq!(report.failed_runs, 0, "smoke seed failed: {report}");
assert!(
    report.assertion_violations.is_empty(),
    "safety property failed: {report}"
);
```

`SimulationBuilder::run()` is synchronous; do not add `.await` or create a
Tokio runtime around it.

No explicit `Chaos` entry means the runner uses its baseline network and
storage configurations. This is not the same as a perfectly inert network:
the current baseline network includes seeded timing variation, probabilistic
connect failure, partial I/O, rare corruption/close, clock drift, and
buggified delay, while baseline storage faults are off. `Chaos::Network` and
`Chaos::Storage` select per-seed randomized or swarm configurations.

If a process topology matters, replace `.processes` with `.cluster` and add
`.link_latency`. If the system has several kinds of server (acceptors and
matchmakers, a tier and a spare pool), call `.processes` once per role: each
call is an independent group named after its process type, with its own
per-seed count and its own `10.0.{group}.x` IP range, queried with
`ctx.topology().ips_in_group("name")`; a process's `client_id` /
`client_count` are its index within and the size of its own group. If roles
inside one group matter, call `.tags(...)` after that group's registration;
remember that `.tags` returns a `Result`.

Exit criterion: the seed succeeds repeatedly and the report has no safety
violations.

## Stage 5: Add the Correctness Oracle Before More Chaos

Use four complementary oracles:

1. Return `SimulationError` when an operation cannot complete its test
   contract.
2. Use always-type macros for local facts.
3. Emit stable tracing facts and use `Invariant` for global, cross-process,
   cross-time safety.
4. Make the final live query in `Workload::run`, then use `Workload::check` for
   retained model/timeline assertions after processes stop.

### Local safety

```rust,ignore
moonpool_sim::assert_always!(
    applied_index <= committed_index,
    "replica: applied index never exceeds commit index",
    { "committed" => committed_index, "applied" => applied_index }
);
```

Moonpool's assertion macros record failures; they do not immediately panic.
The simulation report is the authority. Never delete or weaken a failing
assertion to make a run pass.

### Global safety through tracing

Emit plain INFO-or-higher tracing events inside a `Process` or `Workload` task:

```rust,ignore
tracing::info!(
    target: "protocol",
    slot,
    ballot,
    value = %value_hash,
    "value_chosen"
);
```

The event message must be a non-empty constant name. Use `%` for string values.
The run's subscriber is floored at `INFO` by default, so `DEBUG`/`TRACE` spans
and events (`#[tracing::instrument(level = "trace")]` on hot paths) cost one
level compare and are never allocated; `.trace_level(LevelFilter::DEBUG)` on
the builder lowers the floor to debug one seed, capturing those events too.
Do not add simulated time manually; the observability layer stamps it. Actor
spans provide source attribution automatically.

Built-in network, storage, and process faults join the same timeline as
`"sim_fault"` events (`SIM_FAULT_EVENT_NAME`), with `source = "sim"` and a
`kind` field. An invariant can use a separate cursor for that event stream to
correlate a timeout, reboot, corruption, or partition with later application
facts. This is usually more reliable than reconstructing fault history from
formatted logs.

An invariant reads only new events using a cursor and resets its own state for
the next timeline:

```rust,ignore
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use moonpool_sim::{Invariant, TraceQuery};

struct Agreement {
    cursor: Cell<usize>,
    chosen: RefCell<BTreeMap<u64, String>>,
}

impl Agreement {
    fn new() -> Self {
        Self {
            cursor: Cell::new(0),
            chosen: RefCell::new(BTreeMap::new()),
        }
    }
}

impl Invariant for Agreement {
    fn name(&self) -> &str {
        "agreement"
    }

    fn observe(&self, query: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut chosen = self.chosen.borrow_mut();
        for event in query.since("value_chosen", &self.cursor) {
            let Some(slot) = event.u64("slot") else {
                moonpool_sim::assert_unreachable!(
                    "protocol: value_chosen event missing slot"
                );
                continue;
            };
            let Some(value) = event.str("value") else {
                moonpool_sim::assert_unreachable!(
                    "protocol: value_chosen event missing value"
                );
                continue;
            };
            if let Some(previous) = chosen.get(&slot) {
                moonpool_sim::assert_always!(
                    previous == value,
                    "protocol: one chosen value per slot"
                );
            } else {
                chosen.insert(slot, value.to_owned());
            }
        }
    }

    fn reset(&mut self) {
        self.cursor.set(0);
        self.chosen.get_mut().clear();
    }
}
```

Register it with `.invariant(Agreement { ... })`. Invariants run after every
simulation step, so keep them cursor-based and inexpensive. Treat `observe` as
read-only; do not emit new tracing facts or mutate the system from it.

Exit criterion: the harness can detect a deliberately planted wrong result.
Remove the planted bug, not the oracle.

## Stage 6: Choose Guidance Macros Deliberately

Antithesis describes assertions as both properties and clues for search. Its
guidance is useful here too: line coverage says a location ran, while a
sometimes assertion describes a meaningful *situation*. Moonpool uses the
successful encounters, watermarks, and frontiers below as semantic replay
anchors.

| Intent | Macro | Search/report behavior |
|---|---|---|
| A condition at a regularly evaluated site must always hold | `assert_always!` | Safety violation on any false encounter |
| A condition must hold if an optional path executes | `assert_always_or_unreachable!` | Safety violation only when reached and false |
| A path must never execute | `assert_unreachable!` | Safety violation when reached |
| A boolean situation should occur | `assert_sometimes!` | First true encounter is a discovery |
| Mark an already-entered path as interesting | `assert_reachable!` | Encounter is a discovery |
| A numeric bound must always hold | `assert_always_{greater,less}_than...!` | Numeric safety violation |
| Drive a quantity toward a numeric goal | `assert_sometimes_{greater,less}_than...!` | Every better `left - right` comparison distance can guide deeper search |
| Several conditions should coincide | `assert_sometimes_all!` | Guides on frontier advances and new partial truth combinations |
| Explore a bounded vocabulary of states | `assert_sometimes_each!` | New identity bucket and better per-bucket quality guide search |

The exact numeric safety macros are
`assert_always_greater_than!`,
`assert_always_greater_than_or_equal_to!`,
`assert_always_less_than!`, and
`assert_always_less_than_or_equal_to!`. The exact numeric guidance macros use
the same four suffixes after `assert_sometimes_`. Do not invent shortened
spellings.

### Stable messages are property IDs

Assertion slots are keyed by the message hash, not by file and line. Use one
short, stable, human-readable message for one property. Do not interpolate a
node ID, ballot, slot, key, or seed into the message. Do not reuse one message
for different macro kinds. Messages are stored in fixed-size shared slots, so
put dynamic context in supported detail fields or tracing events instead.
Detail maps are supported by boolean always/optional-always/unreachable and
numeric always macros; the sometimes guidance macros do not accept them.

Unlike Antithesis's build-time assertion catalog, Moonpool allocates an
assertion slot only when the macro executes. Consequently:

- `assert_reachable!("retry entered")` is an anchor once the retry path runs;
  it cannot report a path that was never executed because no slot exists yet.
- To prove a condition from an always-executed location, use
  `assert_sometimes!(retry_count > 0, "client: retry occurred")`.
- A sometimes site that never executes is likewise unknown. Put important
  coverage checks at a stable observation point, not only inside the rare arm.
- An adaptive run with no observed boolean sometimes/reachable slot cannot
  satisfy its “all reached” gate and will run to the iteration cap. Add at
  least one repeatedly evaluated boolean coverage contract to a campaign.

### Boolean guidance

Use `assert_sometimes!` for a single meaningful state:

```rust,ignore
moonpool_sim::assert_sometimes!(
    retry_count > 0,
    "client: request completed after retry"
);
```

Use `assert_sometimes_all!` when the hard part is making several facts true at
the same time. Name each proposition; all expressions are evaluated.

```rust,ignore
moonpool_sim::assert_sometimes_all!("protocol: failover completed", [
    ("old leader unavailable", old_leader_unavailable),
    ("new quorum formed", new_quorum_formed),
    ("client completed", client_completed),
]);
```

This macro guides on frontier improvement: a run with two of three facts is
more useful than a run with one. It also distinguishes bounded partial truth
combinations, so "only old leader unavailable" and "only new quorum formed"
can both become replayable states even though each has frontier one. The final
report and adaptive coverage gate remain incomplete until all propositions are
true together; do not duplicate it with a separate boolean completion check.

Partial combinations use a 64-bit per-site bitmap. This is bounded guidance,
not exhaustive subset accounting: collisions may merge optional hints. Use
`assert_sometimes_each!` with a deliberately small identity vocabulary when
exact per-value reporting matters.

### Numeric guidance

Use numeric sometimes macros for depth. A boolean such as `depth > 3` produces
one discovery; a maximizing watermark can keep producing useful discoveries as
depth improves.

```rust,ignore
moonpool_sim::assert_sometimes_greater_than!(
    decided_slots,
    32,
    "protocol: decided slot watermark"
);

moonpool_sim::assert_sometimes_less_than!(
    healthy_replicas,
    3,
    "protocol: low healthy replica watermark"
);
```

Choose direction carefully. Greater/greater-or-equal guidance maximizes
`left - right`; less/less-or-equal guidance minimizes it. The human report
still displays the best observed left operand. Tracking the distance means a
dynamic threshold cannot report false progress merely because both operands
grew. Values are converted to `i64`, so avoid relying on larger unsigned
ranges.

The first numeric evaluation establishes a watermark discovery even when the
comparison has not passed yet. Later improvements can therefore guide a search
gradually from 1 toward a target of 100.

### Bucketed guidance

Use `assert_sometimes_each!` to explore a small semantic vocabulary:

```rust,ignore
moonpool_sim::assert_sometimes_each!(
    "protocol state",
    [("phase", phase_code), ("fault regime", fault_regime)],
    [("decided slots", decided_slots)]
);
```

Identity values, in their supplied order, decide which bucket this is; key
names are descriptive and do not change identity. Quality keys rank better
exemplars inside that bucket. Keep identity coarse: phase, role, recovery mode,
quorum shape, or fault regime. Put progress such as decided slots, inverse lag,
or remaining health in quality.

The shared tables are bounded: 512 assertion sites and 256
`assert_sometimes_each!` buckets. Supply no more than six identity keys; extra
values affect the bucket hash but are not retained for display. Never bucket
unbounded keys, request IDs, ballots, or log slots. Bucket them into a finite
range first. Once a shared table is full, additional sites or buckets are not
tracked, so table exhaustion silently destroys guidance.

Quality always maximizes, supports at most four values, and packs the low 16
bits of each value in order. Normalize values into `0..=32767`, put the most
important value first, and invert a cost when lower is better.

Bucket guidance has no final “all expected identities were visited” contract.
Inspect `report.bucket_summaries` or add stable outer `assert_sometimes!`
contracts for states that the campaign must reach.

Exit criterion: every important rare state has a stable semantic signal, and
the signals describe situations rather than implementation lines.

## Stage 7: Add Buggify at High-Level Danger Points

Random network and disk faults naturally exercise low-level failure handling.
They may take an enormous number of trials to create a high-level dangerous
sequence such as two minimal-quorum elections followed by a retry. Buggify lets
the application cooperate with the simulator by making those valid but rare
choices directly.

Moonpool's model has two decisions:

1. On first encounter, each source `file:line` Buggify location is activated or
   disabled for the whole timeline. `SimulationBuilder` currently uses 50%
   activation.
2. An active location fires on each call with the macro probability: 25% for
   `buggify!()` and `buggify_knob!()`, or the argument to
   `buggify_with_prob!(p)`.

The first encounter of a default point therefore has a 12.5% marginal firing
chance. If the point is activated, later calls each have a 25% chance; if it is
disabled, it never fires in that timeline. Keep custom probabilities in
`0.0..=1.0`.

Buggify is initialized for every builder iteration independently of
`.enable_chaos(...)`. There is no builder switch for ordinary call-site
Buggify. Outside an active simulation iteration it returns false. Moving a
Buggify call to a different line changes its identity and seeded behavior.
Adding, removing, or reordering Buggify points also changes simulation RNG
consumption and can invalidate exact recipes against the changed binary. Do
not call `buggify_init` or `buggify_reset` from a builder-managed workload.
Two macro expansions on the same `file:line` share one activation identity.

Start with one point and pair it with a coverage signal and a safety oracle:

```rust,ignore
let use_minimal_quorum = moonpool_sim::buggify!();
moonpool_sim::assert_sometimes!(
    use_minimal_quorum,
    "protocol: minimal quorum path selected"
);

let responders = if use_minimal_quorum {
    quorum_size
} else {
    all_replicas
};
```

Buggify itself is not an explorer discovery. The paired sometimes assertion,
reachability marker, watermark, frontier, or bucket creates the semantic
anchor after the injected behavior becomes meaningful.

Good Buggify candidates are:

- perform only the minimum correct amount of optional work;
- substitute a retryable error after a lower layer succeeded, forcing
  ambiguous-result handling;
- add or lengthen a delay through `ctx.time()` at a concurrency-sensitive
  boundary;
- choose a valid edge configuration, such as a batch size of one;
- activate an alternate supported path that defaults rarely or never.

Use `buggify_with_prob!(p)` when a frequently evaluated point would otherwise
dominate the run. Use `buggify_knob!(default, lo..hi)` for an application knob:

```rust,ignore
let batch_size = moonpool_sim::buggify_knob!(128_usize, 1_usize..16_usize);
```

Always supply a valid, non-empty half-open range. A fired invalid range falls
back to `range.start`, not to the default value.

`Chaos::BuggifyKnobs` is different. It asks the builder to spike selected
built-in network/storage configuration values, and it only modifies a surface
that is also explicitly enabled:

```rust,ignore
.enable_chaos([
    Chaos::Network(ChaosMode::Swarm),
    Chaos::Storage(ChaosMode::Swarm),
    Chaos::BuggifyKnobs,
])
```

Do not use Buggify to invent impossible states, violate the protocol's own
preconditions, or simulate a process crash. Use built-in attrition for
graceful, crash, and crash-and-wipe lifecycle behavior. Keep injection code in
simulation-specific adapters or behind the application's simulation feature so
production builds can omit `moonpool-sim` and the explorer.

The practical rule from FoundationDB-style Buggify is “do bad things, but not
too many of them.” Preserve enough successful work for the system to make
progress and recover. A chaos phase should eventually stop injecting
background lifecycle faults so the workload can settle and validate recovery.

Exit criterion: each Buggify point is observed across seeds, has a named reason
to find bugs, and leaves the system in a state the real implementation could
encounter.

## Stage 8: Layer Runner Chaos One Axis at a Time

Once the oracle catches planted bugs and the smoke seed is stable, expand
breadth gradually:

1. Multiple baseline/default seeds.
2. Network `Random` mode.
3. Storage `Random` mode if the system persists state.
4. Attrition with at most one unavailable process.
5. `Swarm` mode for each fault surface.
6. Workload operation swarming.
7. Correlated locality failures and distance latency if deployment shape
   matters.

```rust,ignore
use std::time::Duration;

use moonpool_sim::{Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode};

let report = SimulationBuilder::new()
    .processes(3, || Box::new(Node))
    .workload_factory(|| Box::new(ClientWorkload::new(200)))
    .chaos_duration(Duration::from_secs(30))
    .enable_chaos([
        Chaos::Network(ChaosMode::Swarm),
        Chaos::Storage(ChaosMode::Swarm),
        Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 1.0,
                prob_crash: 2.0,
                prob_wipe: 0.0,
                recovery_delay_ms: None,
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
            },
            mode: ChaosMode::Swarm,
        },
        Chaos::BuggifyKnobs,
    ])
    .until_coverage_stable(10, 5_000)
    .run();
```

The attrition `prob_*` values are weights, not percentages. Wipe is only valid
when the model permits losing that process's durable state. Failure-domain
attrition needs `.cluster(...)` and a budget large enough to remove the chosen
machine, zone, or datacenter. With several process groups, set `victims` to
`AttritionVictims::group("acceptor")` (or `::tagged(key, value)`) so kills land
on the role under test rather than on spares; `max_dead` then counts dead
processes in that pool only, and one `Chaos::Attrition` entry per group gives
each role its own regime and budget.

`.chaos_duration(...)` bounds the window in which Moonpool may inject **new**
faults of any kind. At the cutoff the runner stops attrition and custom fault
injectors *and* calls `SimWorld::enter_recovery_mode()`, which switches off every
configuration-driven network, storage, and block-device fault family and heals
the partitions in force.

It stops fault generation, it does not repair anything: corrupted sectors, lost
or misdirected writes, connections already closed, and processes already killed
all survive the cutoff, and finite effects already started (disk stall/throttle
episodes, clogs, delayed packets) expire on their own schedule. The cluster is
not healthy at the cutoff — the environment merely stops making it worse.

Leave a recovery segment inside `Workload::run` after the cutoff for
reconvergence and final live queries: that quiet tail, with the processes still
alive, is the only window where protocol recovery can happen (settle runs after
the processes are aborted). Then use `check` for retained state and timeline
assertions.

### Swarm the operation alphabet

Faults can suppress one another, and so can workload actions. Assign stable
`u8` IDs to operation families and filter them with `swarm_op_enabled`:

```rust,ignore
const PUT: u8 = 0;
const GET: u8 = 1;
const DELETE: u8 = 2;

let mut enabled: Vec<u8> = [PUT, GET, DELETE]
    .into_iter()
    .filter(|op| moonpool_sim::swarm_op_enabled(*op))
    .collect();
if enabled.is_empty() {
    enabled.extend([PUT, GET, DELETE]);
}
```

Opt in with `.swarm_operations()`. Keep the operation picker RNG discipline
stable: consume the same number of simulation RNG draws per logical step where
practical. This keeps exploration recipe coordinates meaningful when the
enabled alphabet changes.

Exit criterion: each chaos axis has passed alone before the combined campaign
is trusted.

## Stage 9: Configure Seeds and Stop Conditions for the Job

Use the runner mode that matches the current stage:

| Task | Configuration |
|---|---|
| Smoke test | one fixed seed plus `.set_iterations(1)` |
| Reproduce ordinary failure | `.set_debug_seeds(vec![seed])` and one iteration |
| Main campaign | default or explicit `.until_coverage_stable(plateau, cap)` |
| Fixed-size CI campaign | `.set_iterations(n)` |
| Replay explorer failure | `.replay_timeline(seed, recipe)` |

`UntilCoverageStable` is the builder default: 10 quiet seeds and a 1,000-seed
cap. Under `cargo xtask sim run`, LLVM sanitizer coverage supplies the plateau
signal. Under ordinary test execution, Moonpool falls back to reached
sometimes/reachable assertion slots. `report.saturation` names the signal.

`set_debug_seeds` supplies seeds but does not change the iteration limit; pair
it with `.set_iterations(seeds.len())`. Campaign seeds that are not supplied
explicitly start from wall-clock entropy. Always retain `report.seeds_used` and
`report.seeds_failing` so individual timelines remain reproducible.

Set `.run_time_budget(...)` to catch self-perpetuating simulated-time loops;
the default is intentionally generous.

A strict campaign binary should print the report and fail CI on all of these:

```rust,ignore
report.eprint();

let failed = report.failed_runs > 0
    || !report.assertion_violations.is_empty()
    || !report.coverage_violations.is_empty()
    || report.convergence_timeout;

if failed {
    std::process::exit(1);
}
```

For a deliberately short smoke or single-seed replay, coverage misses are
expected; fail only on run and safety failures there. Never silently accept
`convergence_timeout` in the campaign intended to establish coverage.

Run simulation binaries through xtask:

```bash
nix develop --command cargo xtask sim list
nix develop --command cargo xtask sim run my-system
```

Exit criterion: the report policy distinguishes safety failure, coverage miss,
and saturation timeout.

## Stage 10: Enable Exploration Last

Exploration adds depth after the deterministic lifecycle, oracle, and semantic
signals are trustworthy. It replays every timeline from the beginning, reaches
an assertion discovery coordinate, and then changes the counted simulation RNG
for the continuation.

Start in deterministic in-process mode:

```rust,ignore
use moonpool_sim::ExplorationConfig;

let report = SimulationBuilder::new()
    .processes(3, || Box::new(Node))
    .workload_factory(|| Box::new(ClientWorkload::new(200)))
    .invariant(Agreement::new())
    .enable_exploration(ExplorationConfig {
        workers: 0,
        max_runs_per_seed: 2_000,
        branching_factor: 4,
        max_frontier: 512,
        max_recipe_len: 32,
    })
    .until_coverage_stable(10, 1_000)
    .run();
```

Exploration rejects `.workload(instance)` because it cannot be reconstructed
with fresh state. Use workload factories; fault injectors are always
factory-built (`fault_factory` and the built-in `Chaos` surfaces). Increase `workers` only
in a standalone single-threaded simulation binary after the fork boundary has
been audited.

Assertion discoveries guide the frontier. Sanitizer code coverage is a report
and plateau signal, not a recipe anchor. Numeric watermarks,
`assert_sometimes_all!`, and bounded `assert_sometimes_each!` states provide
more sustained guidance than many one-shot booleans.

Recipes change the counted simulation RNG; executor scheduling, `select!`
offsets, and configuration/swarm choices still derive from the root seed. Use
many root seeds for scheduling breadth and exploration for semantic depth.

Replay every captured failure with a fresh builder:

```rust,ignore
let Some(bug) = report
    .exploration
    .as_ref()
    .and_then(|exploration| exploration.bug_recipes.first())
else {
    eprintln!("expected an exploration bug recipe");
    std::process::exit(1);
};

let replay = SimulationBuilder::new()
    .processes(3, || Box::new(Node))
    .workload_factory(|| Box::new(ClientWorkload::new(200)))
    .invariant(Agreement::new())
    .replay_timeline(bug.seed, bug.recipe.clone())
    .run();
```

If replay differs, audit external I/O, reused state, unseeded collections,
direct Tokio calls, changed Buggify line locations, and inconsistent RNG draw
counts. See [Exploring a Consensus Protocol](part5-building-on-top/07-exploring-consensus.md)
for consensus-specific signals and operational limits.

Exit criterion: a captured planted failure replays on a fresh cluster before
real findings are trusted.

## Stage 11: Debug in a Narrow Loop

When a safety assertion catches a bug:

1. Stop broad random changes.
2. Record the ordinary failing seed or exploration recipe.
3. Rebuild exactly the same processes, workload factories, invariant state,
   chaos surfaces, and operation alphabet.
4. Run one seed or one recipe at ERROR logging first.
5. Add stable structured tracing around the violated state transition.
6. Trace the full data flow and persistence boundary.
7. Fix the implementation, never the assertion.
8. Replay the exact failure.
9. Restore the full coverage-stable campaign.

Do not turn a discovered bug into a narrow seed-only regression test and stop.
The value of Moonpool is continuing to search the neighboring state space after
the root cause is fixed.

## Agent Definition of Done

Before claiming that a simulation integration is complete, verify all of the
following:

- [ ] Production behavior uses provider traits for task, time, network,
      storage, and randomness.
- [ ] Server state lives in a factory-created `Process`; durable state uses
      simulated storage.
- [ ] Every timeline receives a fresh workload and reference model.
- [ ] The operation alphabet is finite, named, and able to reconcile ambiguous
      results.
- [ ] Local safety uses always-type macros; global safety uses cursor-based
      tracing invariants.
- [ ] Assertion messages are stable, unique, and non-dynamic.
- [ ] Sometimes signals describe meaningful situations, with numeric/frontier/
      bucket guidance for depth.
- [ ] Each Buggify point creates a possible dangerous state and has paired
      coverage plus safety checks.
- [ ] Network, storage, attrition, Buggify knobs, and operation swarm were added
      gradually.
- [ ] The report is checked for run failures, safety violations, intended
      coverage misses, and convergence timeout.
- [ ] Failing seeds and recipes replay from fresh state.
- [ ] The repository's required formatting, Clippy, tests, portability checks,
      simulation binaries, and book build pass.

For this repository, the final validation baseline is:

```bash
nix develop --command cargo fmt
nix develop --command cargo clippy
nix develop --command cargo nextest run
nix develop --command mdbook build book/
```

Run the relevant `cargo xtask sim run <filter>` campaign as well. When crate
features or portability change, also run the no-default-feature and wasm checks
documented in the repository instructions.

## Tested Examples to Read Next

Use current tested code as an API reference instead of inventing one giant
example:

- `crates/moonpool-sim/tests/exploration/tests.rs`: process, simulated TCP,
  tracing invariant, semantic guidance, exploration, and recipe replay.
- `crates/moonpool-sim-examples/src/topology.rs`: topology, provider timeouts,
  and correlated attrition.
- `crates/moonpool-sim-examples/src/tonic_grpc.rs`: production-style transport
  integration and recovery coverage.
- `crates/moonpool-sim-examples/src/dungeon.rs`: fixed-draw operation swarming,
  compound guidance, and quality watermarks.
- `crates/moonpool-sim/tests/chaos/swarm.rs`: layering
  `Chaos::BuggifyKnobs` onto enabled fault surfaces.
- `crates/moonpool/examples/metastable_grpc_retry_storm.rs`: open-loop load over
  gRPC, Prometheus series read back through `MetricQuery`, and an ASCII
  time-series graph printed to stdout.

Then use the detailed chapters on [Process and Workload](part2-foundations/08-process-workload.md),
[assertion concepts](part3-building/13-assertion-concepts.md),
[events and invariants](part3-building/17-events-and-invariants.md), and
[designing workloads](part3-building/19-designing-workloads.md).

## External Design Sources

Moonpool's Buggify placement advice follows the practical patterns documented
in [BUGGIFY](https://transactional.blog/simulation/buggify): do minimal valid
work, force rare error handling, emphasize concurrency, vary knobs, and leave a
recovery phase. The two-stage probabilities above are Moonpool's current
implementation details, not FoundationDB defaults.

The assertion strategy is informed by Antithesis's official guidance on
[asserting correctness](https://antithesis.com/docs/product/writing_tests/assertions/)
and [sometimes assertions](https://antithesis.com/docs/best_practices/sometimes_assertions/):
properties should have stable human-readable identities, and semantic
situations are more useful than undifferentiated line coverage. Moonpool's
fixed shared tables, runtime-only cataloging, and replay-from-start frontier
are Moonpool-specific and are described explicitly above.
