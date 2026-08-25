# Exploring a Consensus Protocol

<!-- toc -->

The explorer is most useful for Paxos, Raft, and replicated logs when it is
given two different kinds of signal: strict safety invariants that catch bugs,
and semantic progress assertions that tell the frontier which prefixes are
worth continuing. Code coverage alone reports that a branch ran; it does not
provide the RNG-coordinate anchor used to build a recipe.

## Start Conservatively

Use fork-free exploration while validating a new integration:

```rust,ignore
use moonpool_sim::{ExplorationConfig, SimulationBuilder, WorkloadCount};

let report = SimulationBuilder::new()
    .processes(3, || Box::new(PaxosNode::new()))
    .workloads(WorkloadCount::Fixed(1), |_| {
        Box::new(PaxosWorkload::new())
    })
    .invariant(AgreementInvariant::default())
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

`workers: 0` makes controller order deterministic and avoids `fork()` while
the test-driver lifecycle is being audited. Increase the worker count only in
a standalone simulation binary on Linux or macOS, after confirming the host
process has no unrelated threads or external resources that a child could
inherit.

Use `.workload_factory(...)` or `.workloads(...)` for the client driver.
Exploration rejects a single `.workload(instance)`, `before_iteration` hooks,
and custom fault-injector instances because those opaque values cannot be
reconstructed for every continuation. Built-in `Chaos` surfaces are supported.
Keep authoritative test state inside fresh processes and factory-created
workloads; do not depend on external clocks, randomness, threads, files, or
sockets outside Moonpool providers.

## Put Safety in Invariants

Emit stable, structured facts from the Paxos implementation:

```rust,ignore
tracing::info!(
    target: "paxos",
    slot,
    ballot,
    value = %value_hash,
    "value_chosen"
);
```

Then check cross-node and cross-time properties with cursor-based invariants.
For Paxos, the minimum useful set is:

- **Agreement:** two chosen events for the same slot never name different
  values.
- **Validity:** every chosen value was proposed by a client.
- **Acceptor persistence:** after a promise or acceptance survives `sync_all`,
  a reboot never reveals an older ballot or a conflicting accepted value.
- **Prefix consistency:** for Multi-Paxos, committed logs on any two nodes agree
  throughout their common prefix.
- **Client semantics:** retries do not create two successful outcomes for one
  non-idempotent command.

The agreement example in [Events and Invariants](../part3-building/17-events-and-invariants.md)
shows the full `TraceQuery::since` pattern. Run these checks after every
simulation step; do not wait until the workload finishes, because a later
state can hide a transient safety violation.

## Add Semantic Replay Anchors

Use discovery assertions for qualitatively different protocol states:

```rust,ignore
assert_sometimes!(leader_changed, "paxos leader changed");
assert_sometimes!(promise_rejected, "stale ballot was rejected");
assert_sometimes!(recovered_acceptor, "acceptor recovered persisted state");

assert_sometimes_all!("failover made progress", [
    ("old leader unavailable", old_leader_unavailable),
    ("new quorum formed", new_quorum_formed),
    ("client completed", client_completed),
]);
```

Use watermarks for monotonic depth. They let later runs improve a frontier
instead of consuming one static discovery:

```rust,ignore
assert_sometimes_greater_than!(decided_slots, 32, "many slots decided");
assert_sometimes_greater_than!(ballot_changes, 4, "repeated ballot changes");
assert_sometimes_greater_than!(retry_depth, 3, "deep client retry chain");
```

Use `assert_sometimes_each!` for a small, bounded state vocabulary:

```rust,ignore
assert_sometimes_each!(
    "paxos transition",
    [("phase", phase_code), ("fault regime", fault_regime_code)],
    [("decided slots", decided_slots)]
);
```

Keep identity keys coarse. The shared table holds 512 assertion sites and 256
`sometimes_each` buckets. Bucketing every node, ballot, slot, and value tuple
will exhaust it quickly and teaches the controller identifiers rather than
protocol structure. Good identity dimensions are phase, role, quorum state,
recovery mode, and fault regime; good quality values are decided-slot count,
ballot depth, retry depth, and inverse replica lag.

## Combine Exploration with Broad Seeds

Exploration is semantic-guided randomized testing, not exhaustive model
checking or a proof of Paxos safety. Recipes change the counted simulation RNG
after an anchor, while executor and `select!` ordering remain derived from the
root seed. Discovery latches are cumulative across seeds, so static boolean
milestones mostly guide the first seed that reaches them.

Keep broad multi-seed testing enabled. Dynamic buckets and watermarks let later
seeds contribute new semantic progress, while `UntilCoverageStable` samples
new root schedules until assertion and code coverage plateau.

## Replay Every Failure

An exploration bug marks its root seed as failed and appears in
`report.exploration.bug_recipes`. Reproduce it with a fresh builder and fresh
driver state:

```rust,ignore
let bug = report
    .exploration
    .as_ref()
    .and_then(|exploration| exploration.bug_recipes.first())
    .expect("exploration found a bug recipe");

let replay = SimulationBuilder::new()
    .processes(3, || Box::new(PaxosNode::new()))
    .workloads(WorkloadCount::Fixed(1), |_| {
        Box::new(PaxosWorkload::new())
    })
    .invariant(AgreementInvariant::default())
    .replay_timeline(bug.seed, bug.recipe.clone())
    .run();

assert_eq!(replay.failed_runs, 1);
```

If replay does not reproduce, audit the integration for direct Tokio calls,
unseeded collections or randomness, reused workload state, `before_iteration`
state, and external I/O. A recipe is exact only when the entire test harness is
deterministic and starts each timeline from the same logical state.

## Current Operational Limits

Before treating explorer results as a CI gate for a consensus system, account
for these limits:

- Forked workers have no independent wall-clock deadline; blocking code in a
  child can stall the controller. Moonpool's simulated-time budget only helps
  when the deterministic executor continues making events.
- Parallel workers may discover shared assertion novelty in finish order, so
  search order is not reproducible even though each captured recipe is.
- A worker crash after claiming a discovery can lose that discovery's journal
  anchor for the rest of the run.
- Sanitizer coverage is a reporting and plateau signal, not a frontier anchor.

For a correctness-critical Paxos suite today, use exploration to supplement
the ordinary multi-seed simulation, keep `workers: 0` for deterministic CI,
and replay every captured failure before diagnosing it.
