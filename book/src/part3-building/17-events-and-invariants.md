# Events and Invariants

<!-- toc -->

Assertions live inside workloads. They validate local properties: this key should exist, this response matched what we sent. But some correctness properties span the entire system. The conservation law in a consensus protocol is a good example. All committed values must agree across replicas, and no committed value can be lost. No single workload owns that property. We need something that watches the whole world.

Other properties span time rather than space. Monotonicity says a term number never decreased. Causal ordering says the lock was acquired before the write. Conservation over time says total money stayed constant through a sequence of transfers. A snapshot of the latest state cannot tell us any of these. If a balance went 100, then 50, then 100 again, a snapshot-based check sees 100 and declares everything fine. The dip to 50 is invisible.

Both kinds of properties need the same two ingredients: an append-only timeline of events, and a watcher that runs as the simulation advances.

How would you find a dual leader in production? You would query your traces: every `leader_elected` event, grouped by term, flagged when two nodes claim the same one. Moonpool's invariant system runs exactly that query, against exactly those traces, inside the simulation. The instrumentation you write for production observability **is** the test oracle.

## Emitting Events

Correctness facts are plain `tracing` events. No markers, no derives, no moonpool-specific API:

```rust
// Inside a process or workload — exactly what you'd write for production
// observability anyway.
tracing::info!(target: "raft", term, leader = %my_ip, "leader_elected");
```

Three rules:

1. **The message is the event name.** Use a constant name like `"leader_elected"`, not an interpolated sentence. Events are grouped and queried by name, the same way you would query Loki or your OTel backend.
2. **Use `%` for strings.** `%value` records the `Display` form without quotes. `?value` on a `String` keeps `Debug` quotes, which makes field matching annoying.
3. **`INFO` or above.** `debug!` and `trace!` events are not captured.

That's the whole convention. Sim time is stamped automatically from the simulation clock, so do not include a `time_ms` field.

**Where does the source come from?** The orchestrator wraps every process and workload task in a tracing span carrying its `ip` (`info_span!("process", ip = %ip)`). When an event fires inside that task, the capture layer walks the span scope and attributes the event to the nearest enclosing actor. In production the same role is played by host or pod attributes on your trace resource. Events emitted outside any actor span (runtime internals, the orchestrator itself) are not captured.

Each captured event is a `TraceEvent`:

```rust
pub struct TraceEvent {
    pub seq: u64,        // global monotonic sequence number, reset per seed
    pub time_ms: u64,    // simulation time at capture
    pub source: String,  // ip of the enclosing actor span, or "sim" for faults
    pub target: String,  // tracing target, e.g. "raft"
    pub level: tracing::Level,
    pub name: String,    // the message, e.g. "leader_elected"
    pub fields: BTreeMap<String, FieldValue>,
}
```

The `seq` field gives total ordering across all event names. If `leader_elected` gets seq 0 and `client_ack` gets seq 1, the election was captured first, even at the same `time_ms`. Typed values come out per field: `e.u64("term")`, `e.str("leader")`, `e.bool("ok")`, `e.f64("ratio")` all return `Option`s.

**One emission, two audiences.** In simulation, `SimulationLayer` captures the event into the timeline and invariants cross-validate it. In production, where no `SimulationLayer` is installed, the same event flows to whatever subscriber is configured: `fmt`, OpenTelemetry, structured JSON. Nothing in the emission is moonpool-specific.

## The Invariant Trait

An invariant is a check the orchestrator runs **after every simulation step**. If the invariant records a failure, the simulation reports the failing seed.

```rust
pub trait Invariant: 'static + Send {
    fn name(&self) -> &str;
    fn observe(&self, q: &dyn TraceQuery, sim_time_ms: u64);
    fn reset(&mut self) {}
}
```

The invariant gets a `TraceQuery` view of all captured events plus the current simulation time. The contract: if the property holds, return normally. If it does not, report with `assert_always!` so the failure is recorded with the seed. The `reset` hook is called between seeds to clear cursors and tracking sets. Because steps batch events, one `observe` call may see several new events.

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
SimulationBuilder::new()
    .invariant_fn("commit_volume", |q, _t| {
        assert_always!(q.len("commit") <= 10_000, "runaway commit volume");
    });
```

## Querying the Timeline

`TraceQuery` has three methods. `since(name, &cursor)` returns only the events newer than the cursor and advances it — cheap, and the right default. `snapshot(name)` re-reads everything from the start of the seed. `len(name)` counts.

A real consensus example. Multiple nodes must agree on committed values. The agreement invariant checks that no two nodes have committed different values for the same slot. This catches subtle bugs: a leader change that replays a proposal, a vote that arrives after a new ballot, a crash during the accept phase.

Each node emits `tracing::info!(slot, value, "commit")` whenever it commits. The invariant scans the timeline and verifies no two commits for the same slot disagree.

```rust
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use moonpool_sim::{Invariant, TraceQuery, assert_always};

#[derive(Default)]
pub struct AgreementInvariant {
    cursor: Cell<usize>,
    committed: RefCell<HashMap<u64, u64>>,
}

impl Invariant for AgreementInvariant {
    fn name(&self) -> &str { "agreement" }

    fn observe(&self, q: &dyn TraceQuery, _t: u64) {
        // Cursor-based: only new events since the last call.
        let mut committed = self.committed.borrow_mut();
        for e in q.since("commit", &self.cursor) {
            let slot = e.u64("slot").expect("commit carries a slot");
            let value = e.u64("value").expect("commit carries a value");
            if let Some(prev) = committed.get(&slot) {
                assert_always!(
                    *prev == value,
                    format!("agreement violated at slot {slot}: {prev} vs {value}")
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

If a leader change causes two nodes to commit different values for the same slot, the invariant fires on the step that captured the second commit. Reset between seeds is the invariant's responsibility: clear the cursor in `reset()` and the simulation builder calls it for you between iterations.

The canonical runnable example is [`moonpool-sim/tests/leader_election.rs`](https://github.com/PierreZ/moonpool/blob/main/moonpool-sim/tests/leader_election.rs): a workload emits `leader_elected` events, a `SplitBrainInvariant` detects two leaders claiming the same term. For a deeper one, [`moonpool-transport-sim`](https://github.com/PierreZ/moonpool/tree/main/moonpool-transport-sim) replays a hash chain from `append_block` events, with the block bytes hex-encoded into a string field.

## Reading from a Workload

Inside `Workload::check()` or anywhere you have a `SimContext`, the observability handle implements `TraceQuery` too:

```rust
let q = ctx.observability();
let transfers = q.snapshot("transfer");
let total: u64 = transfers.iter().filter_map(|e| e.u64("amount")).sum();
assert_always!(total <= max_funds, "spent more than reserves");
```

`snapshot` returns an empty `Vec` if nothing was emitted under that name. No `Option` to unwrap.

## The Fault Timeline

Every fault the simulator injects, from network partitions to storage corruption to process kills, lands in the same timeline under the well-known name `"sim_fault"`, with `source = "sim"` and a `kind` field identifying the variant. No workload code needed. These are not production traces — there is no simulator in production — so the engine records them internally and the runner merges them into the timeline after each step (engine-level tests can drain them directly with `SimWorld::take_faults()`).

```rust
use std::cell::Cell;
use moonpool_sim::{Invariant, SIM_FAULT_EVENT_NAME, TraceQuery, assert_always};

pub struct KillRateInvariant {
    cursor: Cell<usize>,
    kill_count: Cell<usize>,
}

impl Invariant for KillRateInvariant {
    fn name(&self) -> &str { "kill_rate" }

    fn observe(&self, q: &dyn TraceQuery, _t: u64) {
        for e in q.since(SIM_FAULT_EVENT_NAME, &self.cursor) {
            if e.str("kind") == Some("process_force_kill") {
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

The `kind` values cover three categories:

- **Process lifecycle**: `process_graceful_shutdown` (with `grace_period_ms`), `process_force_kill`, `process_restart` (all with `ip`)
- **Network**: `partition_created`, `partition_healed` (with `from`/`to`), `connection_cut`, `cut_restored`, `half_open_error`, `random_close`, `peer_crash`, `bit_flip` (with `connection_id`), `send_partition_created`, `recv_partition_created` (with `ip`)
- **Storage**: `storage_read_fault`, `storage_write_fault` (with `write_kind`), `storage_sync_fault`, `storage_crash`, `storage_wipe` (all with `ip`)

The real power is **correlation**. When an application-level invariant fires, cross-reference the fault events to understand what the infrastructure was doing at that moment. A conservation law violation at `t=5000` that coincides with a `process_force_kill` at `t=4980` tells a very different story than one with no faults nearby. Both kinds of events sit in one timeline with one clock, which is exactly how you would correlate an alert window against infrastructure events in a production log aggregator.

## Simulation Time in Log Output

By default, `tracing_subscriber::fmt::layer()` prefixes every log line with wall-clock time. That is exactly wrong for simulation. A seed runs millions of simulated milliseconds inside a few real-time milliseconds, so every line gets the same wall-clock timestamp and reading the log feels like reading a stack trace with the line numbers ripped off.

`SimTime` is a `FormatTime` impl that prints the current sim time in milliseconds reported by a `Clock`. `Clock` is a narrow `Send + Sync` trait, implemented for `SimulationLayerHandle` out of the box, so the typical setup is one line:

```rust
use moonpool_sim::{SimTime, SimulationLayer};
use tracing_subscriber::layer::SubscriberExt;

let sim_layer = SimulationLayer::new();
let handle = sim_layer.handle();

let subscriber = tracing_subscriber::registry()
    .with(sim_layer)
    .with(
        tracing_subscriber::fmt::layer()
            .with_timer(SimTime::new(handle.clone())),
    );

let _guard = tracing::subscriber::set_default(subscriber);
```

Output:

```text
sim+    1.234s  INFO raft: term=3 leader=10.0.1.2 leader_elected
sim+    1.235s  INFO myservice::handler: processing request id=42
```

The orchestrator advances the clock by calling `handle.set_sim_time_ms(...)` after each `sim.step()`. All `tracing::*!` calls between steps, captured or not, see the right time. The handle is per-layer, so two parallel sims keep their clocks isolated.

In production, no `SimulationLayer` is registered and the formatter falls back to its default zero. Use plain wall-clock `fmt::layer()` there, and only pull `SimTime` in for sim and test runs.

## Snapshots vs Timelines

Use both. They solve different problems.

**`StateHandle::publish/get`** stores the latest snapshot. Good for properties about the current state: the sum of all balances equals total deposits minus total withdrawals. Workloads call `ctx.state().publish("model", model.clone())` and read the snapshot at the end of the simulation in `Workload::check`.

**The trace timeline** stores append-only history. Good for properties about how the state changed over time: no message was received before it was sent, the leader term never decreased, every debit has a matching credit.

Invariants can derive snapshots from event histories by keeping an internal counter and updating it as events arrive. The fault timeline adds infrastructure context for free, and every event you emit is one that production observability also sees.

## When to Use Invariants vs Assertions

**Assertions** (`assert_always!`, `assert_sometimes!`) belong inside workloads. They validate local properties from the workload's perspective: this response has the right balance, this error path was exercised.

**Invariants** validate global, cross-workload properties from an omniscient perspective. They watch the trace timeline and check that the system's history is consistent. Use them for:

- Conservation laws (messages, resources, committed values)
- No-phantom properties (never receive something that was not sent)
- Consistency across processes (leader election: at most one leader per term)
- Monotonicity properties (ballot numbers only increase)
- Causal ordering (a write to `x` must come before a read that returns it)
- Time-shaped anomalies (retry rates that stay elevated after the trigger fault healed — the metastable failure signature)

A useful rule of thumb: if the property involves state from more than one process or workload, it is an invariant. If it is about one workload's local view, it is an assertion. Inside an invariant, prefer the assertion macros over a raw `panic!` so failures are recorded with the seed.

## Performance

Invariants run after **every** simulation step. A typical simulation processes thousands of steps per iteration, and you might run hundreds of iterations. Keep invariants fast.

Concretely: use cursor-based `since` rather than full `snapshot`, iterate only the new events, compare a few counters, check a simple predicate. Avoid expensive operations like sorting large datasets or formatting strings on the happy path. The `format!` in the failure message is fine because it only runs when the invariant fails.

If you find yourself wanting a slow invariant, like replaying a log to verify consistency, run it only in the workload's `check()` method at the end of the simulation rather than on every step.
