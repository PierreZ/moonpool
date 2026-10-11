# Exploring a Consensus Protocol

<!-- toc -->

Paxos, Raft and replicated logs are where exploration pays off. Their worst bugs need a sequence: a leader change, then a stale acceptor, then a client retry inside the gap. The explorer finds such sequences when the workload gives it two different signals. **Safety invariants** catch the bug. **Semantic progress assertions** tell the frontier which prefixes are worth continuing. Code coverage reports that a branch ran, but it carries no RNG coordinate, so it never anchors a recipe.

## Start With Workers 0

Validate a new integration without `fork()`:

```rust,ignore
use moonpool_sim::{ExplorationConfig, SimulationBuilder, WorkloadCount};

let report = SimulationBuilder::new()
    .processes(3, || Box::new(PaxosNode::new()))
    .workloads(WorkloadCount::Fixed(1), |_| Box::new(PaxosWorkload::new()))
    .invariant(AgreementInvariant::default())
    .enable_exploration(ExplorationConfig {
        workers: 0,              // deterministic search order, no fork
        max_runs_per_seed: 2_000,
        branching_factor: 4,
        max_frontier: 512,
        max_recipe_len: 32,
    })
    .until_coverage_stable(10, 1_000)
    .run()?;
```

Exploration needs factory workloads (`workload_factory` or `workloads`). A single `.workload(instance)` cannot be rebuilt for every continuation, so `run()` returns `InvalidConfiguration`. Processes are always factory-built, and a custom `FaultInjector` comes through `fault_factory`, so both work under exploration. Raise `workers` only in a standalone simulation binary on Linux or macOS, once the harness has no unrelated threads, sockets or files that a child could inherit.

## Put Safety in Invariants

Emit the protocol's facts as plain `tracing` events from the node code:

```rust,ignore
tracing::info!(target: "paxos", slot, ballot, value = %value_hash, "value_chosen");
```

Then check cross-node, cross-time properties after every simulation step with a cursor-based `Invariant`. The canonical pattern is `crates/moonpool-sim/tests/leader_election.rs`:

```rust,ignore
fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
    let mut seen = self.seen.borrow_mut();
    for e in q.since("leader_elected", &self.cursor) {
        let term = e.u64("term").expect("leader_elected carries a term");
        let leader = e.str("leader").expect("leader_elected carries a leader").to_owned();
        if let Some(prior) = seen.get(&term) {
            // One leader per term. A second claim is split-brain.
            assert_always!(prior == &leader, format!("split-brain at term {term}: {prior} vs {leader}"));
        } else {
            seen.insert(term, leader);
        }
    }
}
```

The minimum useful set for Paxos:

| Property | Check |
|----------|-------|
| Agreement | Two `value_chosen` events for one slot never name different values. |
| Validity | Every chosen value was proposed by a client. |
| Acceptor persistence | After a promise survives `sync_all`, a reboot never reveals an older ballot. |
| Prefix consistency | Committed logs on any two nodes agree on their common prefix. |
| Client semantics | A retry never produces two successful outcomes for one non-idempotent command. |

Run the checks every step. A later state can hide a transient violation that a final check would miss. See [Events and Invariants](../part3-building/17-events-and-invariants.md) for the full pattern.

## Add Semantic Replay Anchors

Discovery assertions name the protocol states the frontier should return to. Only the sometimes family creates discoveries. The always family records violations and a reporting watermark but never guides the search.

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

Watermarks give the explorer monotonic depth. Each improvement is a `WatermarkImprovement` discovery, so a later timeline can push the frontier instead of consuming one static milestone:

```rust,ignore
assert_sometimes_greater_than!(decided_slots, 32, "many slots decided");
assert_sometimes_greater_than!(ballot_changes, 4, "repeated ballot changes");
assert_sometimes_greater_than!(retry_depth, 3, "deep client retry chain");
```

Use `assert_sometimes_each!` for a small, bounded state vocabulary with a quality value:

```rust,ignore
assert_sometimes_each!("paxos transition",
    [("phase", phase_code), ("fault regime", fault_regime_code)],
    [("decided slots", decided_slots)]);
```

Keep identity keys coarse. The shared table holds 2048 assertion sites, 256 `sometimes_each` buckets, and at most 6 keys per site. Bucketing every node, ballot, slot and value exhausts the table and teaches the controller identifiers instead of protocol structure. Good identity dimensions: phase, role, quorum state, recovery mode, fault regime. Good quality values: decided slot count, ballot depth, retry depth, inverse replica lag.

## Combine Exploration With Broad Seeds

Exploration is semantic-guided randomized testing. It is not model checking and not a proof of safety. Discovery latches are cumulative across seeds, so a static boolean milestone guides only the first seed that reaches it. Watermarks and buckets let later seeds still contribute progress.

Keep the default `UntilCoverageStable` control. It samples new root profiles (network, storage, attrition regimes) until the coverage signal plateaus, while exploration deepens each root. The two are complementary: breadth across profiles, depth inside each one.

## Replay Every Failure

An exploration bug marks its root seed failed and lands in `report.exploration.bug_recipes`. Reproduce it with a fresh builder before diagnosing it:

```rust,ignore
let bug = report
    .exploration
    .as_ref()
    .and_then(|e| e.bug_recipes.first())
    .expect("exploration found a bug recipe");

let replay = SimulationBuilder::new()
    .processes(3, || Box::new(PaxosNode::new()))
    .workloads(WorkloadCount::Fixed(1), |_| Box::new(PaxosWorkload::new()))
    .invariant(AgreementInvariant::default())
    .replay_timeline(bug.seed, bug.recipe.clone())
    .run()?;

assert_eq!(replay.failed_runs, 1);
```

If the replay does not reproduce, the harness is not deterministic. Audit for direct `tokio::` calls, `tokio::select!` instead of `moonpool_sim::select!`, `HashMap` iteration order, `std::time`, threads, or I/O outside the providers. `SimulationBuilder::check_determinism()` runs each seed twice and reports a `DeterminismViolation` with the first diverging draw.

## Operational Limits

Account for these before using the explorer as a CI gate:

| Limit | Consequence |
|-------|-------------|
| Workers > 0 finish in wall-clock order | Each recipe replays exactly, but the **search order** and the set of found bugs can differ between campaigns. |
| A worker crash loses its journal | The recipe is kept as a crash reproducer, but discoveries it claimed are not expanded. |
| Sancov is a plateau signal only | Code coverage never anchors a recipe. Use assertions for guidance. |
| The 5-minute worker ceiling | A worker blocked outside the simulation is killed and recorded as a crash with its recipe. |

For a correctness-critical consensus suite today: keep `workers: 0` in `cargo nextest`, run the forked binary through `cargo xtask sim run`, and replay every captured failure.
