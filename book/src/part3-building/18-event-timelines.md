# Event Timelines

<!-- toc -->

The [previous chapter](./17-invariants.md) introduced `StateHandle` with publish/get semantics. Workloads publish a snapshot, invariants read it. But snapshots only show the **current** value. The history is gone. If a balance was 100, then 50, then 100 again, a snapshot-based invariant sees 100 and declares everything fine. The dip to 50 is invisible.

Temporal properties need the full sequence. Monotonicity (a term number never decreased), causal ordering (the lock was acquired before the write), conservation over time (total money stayed constant through a sequence of transfers). These require an append-only log, not a mutable register.

That is what event timelines provide.

## Emitting Events

Inside a workload or process, call `ctx.emit()` with a timeline name and an event value:

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

Each entry is automatically wrapped in a `TimelineEntry<T>`:

```rust
pub struct TimelineEntry<T> {
    pub event: T,       // your payload
    pub time_ms: u64,   // simulation time (auto-captured from ctx.time().now())
    pub source: String, // emitter IP (auto-captured from ctx.my_ip())
    pub seq: u64,       // global monotonic sequence number
}
```

The `seq` field deserves attention. It is a single counter shared across **all** timelines in the `StateHandle`. If timeline A gets seq 0 and timeline B gets seq 1, you know A's event was emitted first, even if both have the same `time_ms`. This gives you a total ordering over all events in the simulation.

## Reading Timelines

Workloads read timelines through `ctx.timeline()`. Invariants read them through `state.timeline()`. Both return an `Option<Timeline<T>>` that is `None` if no events have been emitted to that key yet.

```rust
impl Invariant for TransferOrderInvariant {
    fn name(&self) -> &str { "transfer_ordering" }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        let Some(tl) = state.timeline::<TransferEvent>("transfers") else {
            return; // no transfers yet
        };

        // Check monotonicity: transfer times never go backwards
        let entries = tl.all(); // zero-copy borrow
        for window in entries.windows(2) {
            assert_always!(
                window[1].time_ms >= window[0].time_ms,
                format!("transfer time went backwards: {} -> {}",
                    window[0].time_ms, window[1].time_ms)
            );
        }
    }
}
```

`Timeline<T>` has four read methods:

| Method | Returns | Use case |
|--------|---------|----------|
| `all()` | `Ref<Vec<TimelineEntry<T>>>` | Full scan, zero-copy |
| `since(index)` | `Vec<TimelineEntry<T>>` | Incremental processing from a cursor |
| `last()` | `Option<TimelineEntry<T>>` | Most recent event |
| `len()` | `usize` | Count check |

For invariants that run after every event, `since()` avoids rescanning the entire history. Store the cursor between calls and only process new entries.

## The Fault Timeline

Every fault the simulator injects, from network partitions to storage corruption to process kills, is automatically emitted to a well-known timeline called `"sim:faults"`. No workload code needed.

```rust
use moonpool_sim::{SimFaultEvent, SIM_FAULT_TIMELINE};

fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
    let Some(faults) = state.timeline::<SimFaultEvent>(SIM_FAULT_TIMELINE) else {
        return;
    };

    // Count how many times each process was killed
    let kill_count = faults.all().iter()
        .filter(|e| matches!(&e.event, SimFaultEvent::ProcessForceKill { .. }))
        .count();

    assert_always!(
        kill_count <= 10,
        format!("too many kills in one iteration: {}", kill_count)
    );
}
```

`SimFaultEvent` covers 17 fault variants across three categories:

- **Process lifecycle**: `ProcessGracefulShutdown`, `ProcessForceKill`, `ProcessRestart`
- **Network**: `PartitionCreated`, `PartitionHealed`, `ConnectionCut`, `CutRestored`, `HalfOpenError`, `SendPartitionCreated`, `RecvPartitionCreated`, `RandomClose`, `PeerCrash`, `BitFlip`
- **Storage**: `StorageReadFault`, `StorageWriteFault`, `StorageSyncFault`, `StorageCrash`, `StorageWipe`

The real power is **correlation**. When an application-level invariant fires, cross-reference the fault timeline to understand what the infrastructure was doing at that moment. A conservation law violation at t=5000 that coincides with a `ProcessForceKill` at t=4980 tells a very different story than one with no faults nearby.

## Snapshots vs Timelines

Use both. They solve different problems.

**`publish()`/`get()`** stores the latest snapshot. Good for properties about the current state: "the sum of all balances equals total deposits minus total withdrawals." The conservation law invariant from the [previous chapter](./17-invariants.md) is a snapshot invariant.

**`emit()`/`timeline()`** stores append-only history. Good for properties about how the state changed over time: "no message was received before it was sent," "the leader term never decreased," "every debit has a matching credit."

A well-designed simulation typically publishes a reference model for snapshot invariants and emits events for temporal invariants. The fault timeline adds infrastructure context for free.
