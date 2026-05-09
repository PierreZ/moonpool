# System Invariants

<!-- toc -->

Assertions live inside workloads. They validate local properties: "this key should exist," "this response matched what we sent." But some correctness properties span the entire system. The conservation law in a consensus protocol is a good example: all committed values must agree across replicas, and no committed value can be lost. No single workload owns that property. We need something that watches the whole world.

That is what invariants are for.

## The Invariant Trait

An invariant is a check that runs **after every simulation event**. The simulation engine calls it automatically. If the invariant panics, the simulation stops and reports the failing seed.

```rust
pub trait Invariant: 'static + Send {
    fn name(&self) -> &str;
    fn observe(&self, q: &dyn TimelineQuery, sim_time_ms: u64);
    fn reset(&mut self) {}
}
```

The invariant gets a `TimelineQuery` view of all captured events plus the current simulation time. The contract: if the invariant holds, return normally. If it does not, panic with a descriptive message. The `reset` hook is called between seeds to clear cursors and tracking sets.

You register invariants on the builder:

```rust
SimulationBuilder::new()
    .workload(ConsensusWorkload::new(3))
    .invariant(AgreementInvariant::default())
    .invariant(ValidityInvariant::default())
    .run();
```

For quick one-off checks, there is a closure shorthand:

```rust
use std::cell::Cell;
use moonpool_sim::TimelineQueryExt;

let cursor = std::sync::Arc::new(...);  // see chapter on event timelines
SimulationBuilder::new()
    .invariant_fn("single_leader", move |q, _t| {
        let leaders = q.snapshot::<LeaderEvent>("leadership");
        let active = leaders.iter().filter(|e| matches!(e.event, LeaderEvent::Elected { .. })).count()
                   - leaders.iter().filter(|e| matches!(e.event, LeaderEvent::StepDown { .. })).count();
        assert!(active <= 1, "multiple leaders elected at once");
    });
```

## Where Does Invariant State Come From?

Invariants observe **events** (the typed timelines emitted via `ctx.emit`) and the **fault timeline** (auto-populated by the simulator). For the cross-process consistency properties invariants validate, events are the natural data source: every state change a workload makes corresponds to a recorded event.

Workloads can also use `StateHandle::publish/get` for live snapshots that the workload's own `check()` method reads at the end of the simulation. Invariants stick to the event log because it gives a complete history, not just the latest value.

## A Real Example: The Agreement Invariant

Consider a consensus protocol where multiple nodes must agree on committed values. The agreement invariant checks that no two nodes have committed different values for the same slot. This is the kind of property that catches subtle bugs: a leader change that replays a proposal, a vote that arrives after a new ballot, a crash during the accept phase.

Each node emits a `Commit { slot, value }` event whenever it commits. The invariant scans the timeline and verifies no two commits for the same slot disagree.

```rust
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use moonpool_sim::{Invariant, TimelineQuery, TimelineQueryExt, assert_always};

#[derive(Debug, Clone)]
pub struct CommitEvent {
    pub slot: u64,
    pub value: u64,
}

#[derive(Default)]
pub struct AgreementInvariant {
    cursor: Cell<usize>,
    committed: RefCell<HashMap<u64, u64>>,
}

impl Invariant for AgreementInvariant {
    fn name(&self) -> &str { "agreement" }

    fn observe(&self, q: &dyn TimelineQuery, _t: u64) {
        let new = q.since::<CommitEvent>("commits", &self.cursor);
        let mut committed = self.committed.borrow_mut();
        for entry in new {
            let CommitEvent { slot, value } = entry.event;
            if let Some(prev) = committed.get(&slot) {
                assert_always!(
                    *prev == value,
                    format!("agreement violated at slot {}: {} vs {}", slot, prev, value)
                );
            } else {
                committed.insert(slot, value);
            }
        }
    }

    fn reset(&mut self) {
        self.cursor.set(0);
        self.committed.borrow_mut().clear();
    }
}
```

This runs after every single simulation event. If a leader change causes two nodes to commit different values for the same slot, this invariant fires immediately on the second `Commit`.

## When to Use Invariants vs Assertions

**Assertions** (`assert_always!`, `assert_sometimes!`) belong inside workloads. They validate local properties from the workload's perspective: "this response has the right balance," "this error path was exercised."

**Invariants** validate global, cross-workload properties from an omniscient perspective. They watch the event timeline and check that the system's history is consistent. Use them for:

- Conservation laws (messages, resources, committed values)
- No-phantom properties (never receive something that was not sent)
- Consistency across processes (leader election: at most one leader at any time)
- Monotonicity properties (ballot numbers only increase)
- Causal ordering (a write to `x` must come before a read that returns it)

A useful rule of thumb: if the property involves state from more than one process or workload, it is an invariant. If it is about one workload's local view, it is an assertion.

## Performance

Invariants run after **every** simulation event. A typical simulation processes thousands of events per iteration, and you might run hundreds of iterations. Keep invariants fast.

Concretely: use cursor-based `since` rather than full `snapshot`, iterate only the new entries, compare a few counters, check a simple predicate. Avoid expensive operations like sorting large datasets or doing string formatting on the happy path. The `format!` in the panic message is fine because it only runs when the invariant fails.

If you find yourself wanting a slow invariant (like replaying a log to verify consistency), consider running it only in the workload's `check()` method at the end of the simulation rather than after every event.
