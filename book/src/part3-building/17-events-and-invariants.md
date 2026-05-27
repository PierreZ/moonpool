# Events and Invariants

<!-- toc -->

Assertions live inside workloads. They validate local properties: this key should exist, this response matched what we sent. But some correctness properties span the entire system. The conservation law in a consensus protocol is a good example. All committed values must agree across replicas, and no committed value can be lost. No single workload owns that property. We need something that watches the whole world.

Other properties span time rather than space. Monotonicity says a term number never decreased. Causal ordering says the lock was acquired before the write. Conservation over time says total money stayed constant through a sequence of transfers. A snapshot of the latest state cannot tell us any of these. If a balance went 100, then 50, then 100 again, a snapshot-based check sees 100 and declares everything fine. The dip to 50 is invisible.

Both kinds of properties need the same two ingredients: an append-only log of typed events, and a watcher that runs after every step.

## Emitting Events

Correctness facts are ordinary `tracing` events with three structured fields. Anywhere you have a typed payload that derives `Valuable + Serialize + Deserialize`, fire a normal `tracing::info!`:

```rust
use serde::{Deserialize, Serialize};
use tracing::field::valuable;
use valuable::Valuable;

#[derive(Debug, Clone, Valuable, Serialize, Deserialize)]
struct CommitEvent {
    slot: u64,
    value: u64,
}

// Inside a process or workload
tracing::info!(
    capture = true,
    trail = "commits",
    source = ctx.my_ip(),
    event = valuable(&CommitEvent { slot: 7, value: 42 }),
);
```

Three rules. `capture = true` is the marker that tells `SimulationLayer` this event matters; without it, the layer ignores the event. `trail = "name"` selects the append-only stream that invariants read from. `event = valuable(&payload)` carries the typed payload through `tracing`'s structured field machinery. An optional `source = "..."` records the originating actor; sim time is stamped automatically by the layer, so do not include a `time_ms` field.

For convenience inside a `SimContext`, `ctx.emit(trail, payload)` expands to the same `tracing::info!` with `source = ctx.my_ip()` filled in.

```rust
ctx.emit("commits", CommitEvent { slot: 7, value: 42 });
```

**One emission, two audiences.** In simulation, `SimulationLayer` captures the typed payload and runs invariants. In production, where no `SimulationLayer` is registered, the same event flows to whatever subscriber is configured: `fmt`, OpenTelemetry, structured JSON. The `valuable` machinery keeps the payload's fields visible to every layer; nothing is hidden behind a `Debug` blob.

**Use structs, not unit-variant enums.** `valuable-serde` emits enum unit variants as `{"VariantName": []}` while `serde`'s default external tagging deserializes from the bare string `"VariantName"`, so the round-trip silently fails. Add at least one field (e.g. a `reason: String` for catch-all variants) and the shape is unambiguous.

Each captured entry is wrapped as a `TypedEntry<T>`:

```rust
pub struct TypedEntry<T> {
    pub event: T,        // your payload, deserialized on read
    pub time_ms: u64,    // simulation time at capture
    pub source: String,  // source field (or empty if omitted)
    pub seq: u64,        // global monotonic sequence number
}
```

The `seq` field gives total ordering across all trails. If trail A gets seq 0 and trail B gets seq 1, A's event was emitted first, even at the same `time_ms`.

## The Invariant Trait

An invariant is a check that runs **after every captured event**. The simulation engine calls it automatically. If the invariant panics, the simulation stops and reports the failing seed.

```rust
pub trait Invariant: 'static + Send {
    fn name(&self) -> &str;
    fn observe(&self, q: &dyn TrailQuery, sim_time_ms: u64);
    fn reset(&mut self) {}
}
```

The invariant gets a `TrailQuery` view of all captured events plus the current simulation time. The contract: if the invariant holds, return normally. If it does not, panic with a descriptive message. The `reset` hook is called between seeds to clear cursors and tracking sets. Treat `observe` as read-only: any captured event you try to emit from inside it is silently dropped by `tracing-core`'s dispatch reentrancy guard.

Register invariants on the builder:

```rust
SimulationBuilder::new()
    .workload(ConsensusWorkload::new(3))
    .invariant(AgreementInvariant::default())
    .invariant(ValidityInvariant::default())
    .run();
```

For quick one-off checks there is a closure shorthand:

```rust
use moonpool_sim::TrailQueryExt;

SimulationBuilder::new()
    .invariant_fn("single_leader", |q, _t| {
        let leaders = q.snapshot::<LeaderEvent>("leadership");
        let active = leaders.iter().filter(|e| matches!(e.event, LeaderEvent::Elected { .. })).count()
                   - leaders.iter().filter(|e| matches!(e.event, LeaderEvent::StepDown { .. })).count();
        assert!(active <= 1, "multiple leaders elected at once");
    });
```

## Reading Trails

`TrailQuery` is the dyn-safe core trait. `TrailQueryExt` is auto-implemented for any `TrailQuery` and adds two generic helpers. Use `since::<T>(name, &cursor)` when you only need new entries since the last call (cheap, advances the cursor). Use `snapshot::<T>(name)` when you need a full re-scan.

A real consensus example. Multiple nodes must agree on committed values. The agreement invariant checks that no two nodes have committed different values for the same slot. This catches subtle bugs: a leader change that replays a proposal, a vote that arrives after a new ballot, a crash during the accept phase.

Each node emits a `CommitEvent { slot, value }` whenever it commits. The invariant scans the trail and verifies no two commits for the same slot disagree.

```rust
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use moonpool_sim::{Invariant, TrailQuery, TrailQueryExt, assert_always};

#[derive(Default)]
pub struct AgreementInvariant {
    cursor: Cell<usize>,
    committed: RefCell<HashMap<u64, u64>>,
}

impl Invariant for AgreementInvariant {
    fn name(&self) -> &str { "agreement" }

    fn observe(&self, q: &dyn TrailQuery, _t: u64) {
        // Cursor-based: only new entries since the last call.
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

This runs after every simulation event. If a leader change causes two nodes to commit different values for the same slot, the invariant fires immediately on the second `Commit`. Reset between seeds is the invariant's responsibility. Clear the cursor in `reset()` and the simulation builder calls it for you between iterations.

## Reading from a Workload

Inside `Workload::check()` or anywhere you have a `SimContext`, query the layer directly:

```rust
let entries = ctx.trail::<TransferEvent>("transfers");
let total: u64 = entries.iter().map(|e| e.event.amount).sum();
assert_always!(total <= max_funds, "spent more than reserves");
```

`ctx.trail()` returns `Vec<TypedEntry<T>>`, empty if nothing emitted under that trail. No `Option` to unwrap.

## The Fault Trail

Every fault the simulator injects, from network partitions to storage corruption to process kills, is automatically emitted to a well-known trail called `"sim:faults"`. No workload code needed.

```rust
use std::cell::Cell;
use moonpool_sim::{Invariant, SimFaultEvent, SIM_FAULT_TRAIL, TrailQuery, TrailQueryExt};

pub struct KillRateInvariant {
    cursor: Cell<usize>,
    kill_count: Cell<usize>,
}

impl Invariant for KillRateInvariant {
    fn name(&self) -> &str { "kill_rate" }

    fn observe(&self, q: &dyn TrailQuery, _t: u64) {
        let new = q.since::<SimFaultEvent>(SIM_FAULT_TRAIL, &self.cursor);
        for entry in new {
            if matches!(entry.event, SimFaultEvent::ProcessForceKill { .. }) {
                self.kill_count.set(self.kill_count.get() + 1);
            }
        }
        assert_always!(
            self.kill_count.get() <= 10,
            format!("too many kills: {}", self.kill_count.get())
        );
    }

    fn reset(&mut self) {
        self.cursor.set(0);
        self.kill_count.set(0);
    }
}
```

`SimFaultEvent` covers fault variants across three categories:

- **Process lifecycle**: `ProcessGracefulShutdown`, `ProcessForceKill`, `ProcessRestart`
- **Network**: `PartitionCreated`, `PartitionHealed`, `ConnectionCut`, `CutRestored`, `HalfOpenError`, `SendPartitionCreated`, `RecvPartitionCreated`, `RandomClose`, `PeerCrash`, `BitFlip`
- **Storage**: `StorageReadFault`, `StorageWriteFault`, `StorageSyncFault`, `StorageCrash`, `StorageWipe`

The real power is **correlation**. When an application-level invariant fires, cross-reference the fault trail to understand what the infrastructure was doing at that moment. A conservation law violation at `t=5000` that coincides with a `ProcessForceKill` at `t=4980` tells a very different story than one with no faults nearby.

Because fault events also flow through `tracing`, the same correlation works in production. Capture the same events into a log aggregator and search around the alert window.

## Simulation Time in Log Output

By default, `tracing_subscriber::fmt::layer()` prefixes every log line with wall-clock time. That is exactly wrong for simulation. A seed runs millions of simulated milliseconds inside a few real-time milliseconds, so every line gets the same wall-clock timestamp and reading the log feels like reading a stack trace with the line numbers ripped off.

`SimTime` is a `FormatTime` impl that prints the current sim time in milliseconds reported by a `Clock`. `Clock` is a narrow `Send + Sync` trait, implemented for `SimulationLayerHandle` out of the box, so the typical setup is one line:

```rust
use moonpool_sim::{SimTime, SimulationLayer};
use tracing_subscriber::layer::SubscriberExt;

let sim_layer = SimulationLayer::new();
let handle = sim_layer.handle();

// SimulationLayer must precede fmt in the registry chain so its on_event
// updates the layer's sim time *before* fmt formats the event.
let subscriber = tracing_subscriber::registry()
    .with(sim_layer)
    .with(
        tracing_subscriber::fmt::layer()
            .with_timer(SimTime::new(handle.clone())),
    );

let _guard = tracing::subscriber::set_default(subscriber);
```

`Clock` is the only thing `SimTime` depends on. Implement it for a test stub or an alternate time source if you need one.

Output:

```text
sim+    1.234s  INFO myservice: trail="delivery.at_most_once" event=Replied { seq_id: 7 }
sim+    1.235s  INFO myservice::handler: processing request id=42
sim+    5.000s  WARN moonpool::sim: trail="sim:faults" event=PartitionCreated { from: "10.0.1.1", to: "10.0.1.2" }
```

The orchestrator advances the clock by calling `handle.set_sim_time_ms(sim.current_time())` after each `sim.step()`. All `tracing::*!` calls between events, captured or not, see the right time. The handle is per-layer, so two parallel sims keep their clocks isolated.

In production, no `SimulationLayer` is registered and the formatter falls back to its default zero. Use plain wall-clock `fmt::layer()` there, and only pull `SimTime` in for sim and test runs.

## Snapshots vs Trails

Use both. They solve different problems.

**`StateHandle::publish/get`** stores the latest snapshot. Good for properties about the current state: the sum of all balances equals total deposits minus total withdrawals. Workloads call `ctx.state().publish("model", model.clone())` and read the snapshot at the end of the simulation in `Workload::check`.

**`ctx.emit` and `ctx.trail`** store append-only history. Good for properties about how the state changed over time: no message was received before it was sent, the leader term never decreased, every debit has a matching credit.

Invariants can derive snapshots from event histories by keeping an internal counter and updating it as events arrive. The fault trail adds infrastructure context for free, and every event you emit is one that production observability can also see.

## When to Use Invariants vs Assertions

**Assertions** (`assert_always!`, `assert_sometimes!`) belong inside workloads. They validate local properties from the workload's perspective: this response has the right balance, this error path was exercised.

**Invariants** validate global, cross-workload properties from an omniscient perspective. They watch trails and check that the system's history is consistent. Use them for:

- Conservation laws (messages, resources, committed values)
- No-phantom properties (never receive something that was not sent)
- Consistency across processes (leader election: at most one leader at any time)
- Monotonicity properties (ballot numbers only increase)
- Causal ordering (a write to `x` must come before a read that returns it)

A useful rule of thumb: if the property involves state from more than one process or workload, it is an invariant. If it is about one workload's local view, it is an assertion.

## Performance

Invariants run after **every** simulation event. A typical simulation processes thousands of events per iteration, and you might run hundreds of iterations. Keep invariants fast.

Concretely: use cursor-based `since` rather than full `snapshot`, iterate only the new entries, compare a few counters, check a simple predicate. Avoid expensive operations like sorting large datasets or formatting strings on the happy path. The `format!` in the panic message is fine because it only runs when the invariant fails.

If you find yourself wanting a slow invariant, like replaying a log to verify consistency, run it only in the workload's `check()` method at the end of the simulation rather than after every event.
