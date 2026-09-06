---
name: events-and-invariants
description: Emit correctness facts as tracing events and check cross-process properties with moonpool Invariants - the capture rules (INFO+, constant message, inside a process/workload span), the Invariant trait and invariant_fn, TraceQuery since/snapshot/len with a cursor, TraceEvent field access, sim_fault events, and trace_level. Use when a property spans several processes (split brain, one leader per term, no lost acked write), when a tracing::info! is not showing up in the timeline, or when writing anything that observes the run after each step.
---

# Events and invariants

A per-process `assert_always!` can only see one node. Properties that span
nodes (at most one leader per term, every acked write eventually visible on
every replica) are checked by an `Invariant` that runs after **every**
simulation step over the tracing events the run captured. The instrumentation
is plain `tracing`, exactly what production observability consumes, so the
same query works against a production trace.

## Emit

```rust
tracing::info!(target: "raft", term, leader = %my_ip, "leader_elected");
```

An event is captured only if all three hold:

1. level `INFO` or more severe (the floor is `SimulationBuilder::trace_level`,
   default INFO);
2. the message is a **non-empty constant** (`"leader_elected"`), which is the
   event's name; an interpolated sentence is a different name every time;
3. it fires inside a process or workload task. The orchestrator wraps each in
   an `info_span!("process"/"workload", ip = %ip)`, and the layer resolves
   `TraceEvent::source` from the nearest enclosing span. Events emitted from
   outside those spans are dropped.

Use `%` for strings and IPs (`?` on a `String` keeps Debug quotes); bytes go
hex-encoded into a string field. Do not add a `time_ms` field: the layer
stamps simulated time itself.

Simulator faults are recorded under `SIM_FAULT_EVENT_NAME` (`"sim_fault"`)
with a `kind` field and `source = "sim"`, so an invariant can correlate a
property violation with the fault that preceded it.

## Observe

```rust
use std::cell::{Cell, RefCell};
use moonpool_sim::{Invariant, TraceQuery, assert_always};

struct NoSplitBrain { cursor: Cell<usize>, seen: RefCell<BTreeMap<u64, String>> }

impl Invariant for NoSplitBrain {
    fn name(&self) -> &str { "no_split_brain" }
    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        for e in q.since("leader_elected", &self.cursor) {       // only the new events; accessors return Option
            let (Some(term), Some(leader)) = (e.u64("term"), e.str("leader")) else { continue };
            let leader = leader.to_owned();
            let mut seen = self.seen.borrow_mut();
            match seen.get(&term) {
                Some(first) => assert_always!(first == &leader, "one leader per term",
                                              { "term" => term, "first" => first, "second" => leader }),
                None => { seen.insert(term, leader); }
            }
        }
    }
    fn reset(&mut self) { self.cursor.set(0); self.seen.borrow_mut().clear(); }   // per seed
}

// .invariant(NoSplitBrain::new())  or  .invariant_fn("name", |q, t| { .. })
```

- `since(name, &cursor)` is O(new events); `snapshot(name)` and `len(name)`
  re-read everything and turn a per-step check into O(trace²) over a run.
  Keep a cursor per event name.
- `observe` runs from the runner loop, not from tracing dispatch, and must be
  read-only over the world: it records with the assertion macros, never
  `panic!`, never mutates process state.
- `reset` is called at the start of each seed; forgetting it leaks one seed's
  facts into the next and produces phantom violations.
- Inside a process or workload, `ctx.observability()` implements `TraceQuery`
  too, for a workload's `check()` that wants to read what the cluster
  logged.

## Facts versus scanning

Emit a fact once, at the instant it becomes true, where the matching log line
already is. An invariant that has to infer a fact by scanning several other
events is a sign the fact should be its own event. Tracing is for humans and
for these queries; nothing else should read the trace back.

Canonical example: `crates/moonpool-sim/tests/leader_election.rs`. Sources:
`crates/moonpool-sim/src/observability/{invariant,query,mod}.rs`.
Book: `book/src/part3-building/17-events-and-invariants.md`.
