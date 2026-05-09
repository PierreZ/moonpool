# Event Timelines

<!-- toc -->

The [previous chapter](./17-invariants.md) introduced `StateHandle` with publish/get semantics. Workloads publish a snapshot, invariants read it. But snapshots only show the **current** value. The history is gone. If a balance was 100, then 50, then 100 again, a snapshot-based invariant sees 100 and declares everything fine. The dip to 50 is invisible.

Temporal properties need the full sequence. Monotonicity (a term number never decreased), causal ordering (the lock was acquired before the write), conservation over time (total money stayed constant through a sequence of transfers). These require an append-only log, not a mutable register.

That is what event timelines provide. And the same emission code path also feeds production logs and OpenTelemetry, so the bug you find in simulation is observable when it happens in production too.

## Emitting Events

Inside a workload or process, call `ctx.emit()` with a timeline name and a typed event payload:

```rust
#[derive(Debug, Clone)]
struct TransferEvent {
    from: String,
    to: String,
    amount: u64,
}

// Inside a workload's run() method
ctx.emit("transfers", TransferEvent {
    from: "alice".into(),
    to: "bob".into(),
    amount: 50,
});
```

Under the hood, `ctx.emit()` fires a `tracing::event!` at target `"moonpool::sim"`. In simulation, the registered `SimulationLayer` captures the typed payload, simulation time, and source IP into a per-key vector. In production, where no `SimulationLayer` is registered, the same event flows to whatever subscriber is configured — `fmt`, OpenTelemetry, structured JSON. **One emission, two audiences**: invariants in simulation, observability tooling in production.

The payload type must be `Send + Sync + Debug + 'static`. The macro stashes a typed `Arc<dyn Any>` for the layer to recover; production subscribers get the `Debug` representation.

Each captured entry is wrapped as a `TypedEntry<T>`:

```rust
pub struct TypedEntry<T> {
    pub event: T,        // your payload
    pub time_ms: u64,    // simulation time at emit
    pub source: String,  // emitter IP (or "sim" for simulator-emitted faults)
    pub seq: u64,        // global monotonic sequence number
}
```

The `seq` field gives total ordering across all timelines in the simulation. If timeline A gets seq 0 and timeline B gets seq 1, A's event was emitted first, even at the same `time_ms`.

## Reading Timelines from an Invariant

Invariants implement the `Invariant` trait and receive a `TimelineQuery`:

```rust
use std::cell::Cell;
use moonpool_sim::{Invariant, TimelineQuery, TimelineQueryExt, assert_always};

pub struct TransferOrderInvariant {
    cursor: Cell<usize>,
}

impl Invariant for TransferOrderInvariant {
    fn name(&self) -> &str { "transfer_ordering" }

    fn observe(&self, q: &dyn TimelineQuery, _sim_time_ms: u64) {
        // Cursor-based incremental scan: only see new entries since last call
        let new = q.since::<TransferEvent>("transfers", &self.cursor);
        for entry in new.windows(2) {
            assert_always!(
                entry[1].time_ms >= entry[0].time_ms,
                format!("transfer time went backwards: {} -> {}",
                    entry[0].time_ms, entry[1].time_ms)
            );
        }
    }

    fn reset(&mut self) {
        self.cursor.set(0);
    }
}
```

`TimelineQuery` is the dyn-safe core; `TimelineQueryExt` (auto-implemented for any `TimelineQuery`) adds the generic helpers `since::<T>(key, &cursor)` and `snapshot::<T>(key)`. Use `since` when you only need new entries since the last call (cheap, advances the cursor). Use `snapshot` when you need a full re-scan.

Reset between seeds is the invariant's responsibility — clear the cursor to 0 in `reset()`. The simulation builder calls this for you between iterations.

## Reading Timelines from a Workload

Inside `Workload::check()` or anywhere you have a `SimContext`, query the layer directly:

```rust
let entries = ctx.timeline::<TransferEvent>("transfers");
let total: u64 = entries.iter().map(|e| e.event.amount).sum();
assert_always!(total <= max_funds, "spent more than reserves");
```

`ctx.timeline()` returns `Vec<TypedEntry<T>>` — empty if nothing emitted under that key. No `Option` to unwrap.

## The Fault Timeline

Every fault the simulator injects, from network partitions to storage corruption to process kills, is automatically emitted to a well-known timeline called `"sim:faults"`. No workload code needed.

```rust
use std::cell::Cell;
use moonpool_sim::{Invariant, SimFaultEvent, SIM_FAULT_TIMELINE, TimelineQuery, TimelineQueryExt};

pub struct KillRateInvariant {
    cursor: Cell<usize>,
    kill_count: Cell<usize>,
}

impl Invariant for KillRateInvariant {
    fn name(&self) -> &str { "kill_rate" }

    fn observe(&self, q: &dyn TimelineQuery, _t: u64) {
        let new = q.since::<SimFaultEvent>(SIM_FAULT_TIMELINE, &self.cursor);
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

The real power is **correlation**. When an application-level invariant fires, cross-reference the fault timeline to understand what the infrastructure was doing at that moment. A conservation law violation at `t=5000` that coincides with a `ProcessForceKill` at `t=4980` tells a very different story than one with no faults nearby.

Because fault events also flow through `tracing`, the same correlation works in production: when an alert fires, search your log aggregator for the same `target = "moonpool::sim"` events around the alert window.

## Snapshots vs Timelines

Use both. They solve different problems.

**`StateHandle::publish/get`** stores the latest snapshot. Good for properties about the current state: "the sum of all balances equals total deposits minus total withdrawals." Workloads `ctx.state().publish("model", model.clone())` and read at the end of the simulation in `Workload::check`.

**`ctx.emit` / `ctx.timeline`** stores append-only history. Good for properties about how the state changed over time: "no message was received before it was sent," "the leader term never decreased," "every debit has a matching credit."

Invariants can derive snapshots from event histories — keep an internal `Cell<u64>` count and update it as events arrive. The fault timeline adds infrastructure context for free, and every event you emit is one that production observability can also see.
