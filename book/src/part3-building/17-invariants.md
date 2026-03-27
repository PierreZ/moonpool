# System Invariants

<!-- toc -->

Assertions live inside workloads. They validate local properties: "this key should exist," "this response matched what we sent." But some correctness properties span the entire system. The conservation law in a consensus protocol is a good example: all committed values must agree across replicas, and no committed value can be lost. No single workload owns that property. We need something that watches the whole world.

That is what invariants are for.

## The Invariant Trait

An invariant is a check that runs **after every simulation event**. The simulation engine calls it automatically. If the invariant panics, the simulation stops and reports the failing seed.

```rust
pub trait Invariant: 'static {
    fn name(&self) -> &str;
    fn check(&self, state: &StateHandle, sim_time_ms: u64);
}
```

Two inputs: a `StateHandle` containing shared state that workloads publish, and the current simulation time. The contract is simple: if the invariant holds, return normally. If it does not, panic with a descriptive message.

You register invariants on the builder:

```rust
SimulationBuilder::new()
    .workload(ConsensusWorkload::new(3))
    .invariant(AgreementInvariant)
    .invariant(ValidityInvariant)
    .run()
    .await
```

For quick one-off checks, there is a closure shorthand:

```rust
SimulationBuilder::new()
    .invariant_fn("single_leader", |state, _t| {
        if let Some(model) = state.get::<ConsensusModel>("consensus_model") {
            let leaders: Vec<_> = model.nodes.iter()
                .filter(|(_, s)| s.role == Role::Leader)
                .collect();
            assert!(leaders.len() <= 1, "multiple leaders: {:?}", leaders);
        }
    })
```

## Sharing State with StateHandle

Invariants need to see what workloads are doing. `StateHandle` is the bridge. It is a type-safe, `Rc`-based key-value store that workloads publish into and invariants read from.

Workloads publish their state after each operation:

```rust
// Inside the workload's run() method
self.model.record_commit(slot, value);
ctx.state().publish("consensus_model", self.model.clone());
```

Invariants read it back:

```rust
if let Some(model) = state.get::<ConsensusModel>("consensus_model") {
    // validate...
}
```

The `if let Some` guard is important. Early in the simulation, before the workload has published anything, the key will not exist. Invariants should silently skip when their data is not yet available.

## A Real Example: The Agreement Invariant

Consider a consensus protocol where multiple nodes must agree on committed values. The agreement invariant checks that no two nodes have committed different values for the same slot. This is the kind of property that catches subtle bugs: a leader change that replays a proposal, a vote that arrives after a new ballot, a crash during the accept phase.

```rust
pub struct AgreementInvariant;

impl Invariant for AgreementInvariant {
    fn name(&self) -> &str {
        "agreement"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<ConsensusModel>("consensus_model") {
            for (slot, values) in &model.committed_values {
                let unique: HashSet<_> = values.iter().collect();
                assert_always!(
                    unique.len() <= 1,
                    format!(
                        "agreement violated at slot {}: nodes committed different values {:?}",
                        slot, values
                    )
                );
            }
        }
    }
}
```

This runs after every single simulation event. If a leader change causes two nodes to commit different values for the same slot, this invariant fires immediately.

## When to Use Invariants vs Assertions

**Assertions** (`assert_always!`, `assert_sometimes!`) belong inside workloads. They validate local properties from the workload's perspective: "this response has the right balance," "this error path was exercised."

**Invariants** validate global, cross-workload properties from an omniscient perspective. They see the full system state and check that it is consistent. Use them for:

- Conservation laws (messages, resources, committed values)
- No-phantom properties (never receive something that was not sent)
- Consistency across processes (leader election: at most one leader at any time)
- Monotonicity properties (ballot numbers only increase)

A useful rule of thumb: if the property involves state from more than one process or workload, it is an invariant. If it is about one workload's local view, it is an assertion.

## Performance

Invariants run after **every** simulation event. A typical simulation processes thousands of events per iteration, and you might run hundreds of iterations. Keep invariants fast.

Concretely: iterate a small collection, compare a few counters, check a simple predicate. Avoid expensive operations like sorting large datasets or doing string formatting on the happy path. The `format!` in the panic message is fine because it only runs when the invariant fails.

If you find yourself wanting a slow invariant (like replaying a log to verify consistency), consider running it only in the workload's `check()` method at the end of the simulation rather than after every event.
