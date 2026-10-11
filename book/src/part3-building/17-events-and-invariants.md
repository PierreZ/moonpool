# Events and Invariants

<!-- toc -->

Assertions live inside one workload or one process. They check local facts: this response matches what we sent, this gauge is not negative. Some properties span the whole system. "At most one leader per term" is owned by no single node. "Total money is constant across transfers" needs every account. And some properties span time: a term number never decreased, the lock was taken before the write. A snapshot of the latest state cannot see a balance that went 100, 50, 100.

Both need the same two parts: an append-only timeline of events, and a watcher that runs as the simulation advances.

How would you find a dual leader in production? You would query your traces: every `leader_elected` event, grouped by term, flagged when two nodes claim the same one. Moonpool runs that query, on those traces, inside the simulation. **The instrumentation you ship for observability is the test oracle.**

## Emitting Events

A correctness fact is a plain `tracing` event. No marker, no derive, no moonpool API:

```rust
// Inside a process or workload: what you would write for production anyway.
tracing::info!(target: "raft", term, leader = %my_ip, "leader_elected");
```

Three rules:

1. **The message is the event name.** Use a constant like `"leader_elected"`, not a sentence with values in it. Events are grouped and queried by name.
2. **Use `%` for strings.** `%value` records the `Display` form. `?value` on a `String` keeps the `Debug` quotes.
3. **`INFO` or above.** `debug!` and `trace!` events are not captured by default.

Do not add a `time_ms` field. The layer stamps simulation time itself.

**Where does the source come from?** The orchestrator wraps every process and workload task in a span with its `ip` (`info_span!("process", ip = %ip)`). The capture layer walks the span scope and attributes the event to the nearest enclosing actor. A task spawned with `ctx.task().spawn_task()` runs inside the span that was current at spawn time, as `tokio::spawn` does, so a background handler's events carry its owner's `ip`. Events outside any actor span, from the orchestrator or the runtime, are dropped.

Each captured event is a `TraceEvent`:

```rust
pub struct TraceEvent {
    pub seq: u64,        // global order across all names, reset per seed
    pub time_ms: u64,    // simulation time at capture
    pub source: String,  // ip of the enclosing actor, or "sim" for faults
    pub target: String,  // tracing target, e.g. "raft"
    pub level: tracing::Level,
    pub name: String,    // the message, e.g. "leader_elected"
    pub fields: BTreeMap<String, FieldValue>,
}
```

`seq` orders events across names, even at the same `time_ms`. Typed accessors read fields: `e.u64("term")`, `e.i64("delta")`, `e.f64("ratio")`, `e.bool("ok")`, `e.str("leader")`. Each returns an `Option`.

**One emission, two audiences.** In simulation `SimulationLayer` captures the event and invariants check it. In production no layer is installed, and the same event goes to `fmt`, OpenTelemetry or JSON. Nothing in the call is simulation-specific.

**Below the floor is free.** `SimulationBuilder::run` installs the layer on a `LevelFilter` at the run's trace floor, `INFO` by default. A `DEBUG` or `TRACE` span or event is disabled at the subscriber and costs one level compare. Instrument hot paths with `#[tracing::instrument(level = "trace")]` without fear. To debug one seed, lower the floor with `SimulationBuilder::trace_level(LevelFilter::DEBUG)`. The floor never touches scheduling or randomness, so the seed replays identically at any level. The run is only slower and the capture larger.

## The Invariant Trait

An invariant runs **after every simulation step**:

```rust
pub trait Invariant: 'static + Send {
    fn name(&self) -> &str;
    fn observe(&self, q: &dyn TraceQuery, sim_time_ms: u64);
    fn reset(&mut self) {}
}
```

`observe` gets a `TraceQuery` view of the timeline and the current simulation time. Report a violation with `assert_always!`, never with `panic!`: the macro records the seed and lets the run continue. Steps batch events, so one `observe` call may see several new ones. The runner calls `reset` before each seed to clear cursors and tracking sets.

Register invariants on the builder:

```rust
SimulationBuilder::new()
    .workload(LeaderWorkload::new())
    .invariant(SplitBrainInvariant::new())
    .invariant_fn("commit_volume", |q, _t| {
        assert_always!(q.len("commit") <= 10_000, "runaway commit volume");
    })
    .run()?;
```

`invariant_fn` wraps a closure for quick one-off checks.

## Querying the Timeline

`TraceQuery` has three methods. `since(name, &cursor)` returns the events newer than the cursor and advances it. It is cheap, and the right default. `snapshot(name)` returns everything since the start of the seed. `len(name)` counts.

The canonical example is [`crates/moonpool-sim/tests/leader_election.rs`](https://github.com/PierreZ/moonpool/blob/main/crates/moonpool-sim/tests/leader_election.rs). A workload emits `leader_elected` events, and an invariant checks that no two nodes claim the same term:

```rust
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use moonpool_sim::{Invariant, TraceQuery, assert_always};

struct SplitBrainInvariant {
    cursor: Cell<usize>,
    // term -> first leader seen. Lives across observe() calls within a seed.
    seen: RefCell<BTreeMap<u64, String>>,
}

impl Invariant for SplitBrainInvariant {
    fn name(&self) -> &'static str { "no_split_brain" }

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut seen = self.seen.borrow_mut();
        for e in q.since("leader_elected", &self.cursor) {
            let term = e.u64("term").expect("leader_elected carries a term");
            let leader = e.str("leader").expect("leader_elected carries a leader").to_owned();
            if let Some(prior) = seen.get(&term) {
                // format! is fine here: this branch is the bug, not the hot path.
                assert_always!(
                    prior == &leader,
                    format!("split-brain at term {term}: {prior} vs {leader}")
                );
            } else {
                seen.insert(term, leader);
            }
        }
    }

    fn reset(&mut self) {
        self.cursor.set(0);
        self.seen.borrow_mut().clear();
    }
}
```

The test's bug path emits a second leader for term 1. The invariant fires on the step that captured it, and the report shows `failed_runs >= 1`.

## Reading from a Workload

`ctx.observability()` returns the layer handle, and the handle implements `TraceQuery` too. A `Workload::check` can run the same queries at the end of the seed:

```rust
let q = ctx.observability();
let total: u64 = q.snapshot("transfer").iter().filter_map(|e| e.u64("amount")).sum();
assert_always!(total <= max_funds, "spent more than reserves");
```

`snapshot` returns an empty `Vec` when nothing was emitted under that name.

## The Fault Timeline

Every fault the simulator injects lands in the same timeline under the name `SIM_FAULT_EVENT_NAME` (`"sim_fault"`), with `source = "sim"` and a `kind` field. The engine records them internally (`SimWorld::take_faults()`), and the runner merges them after each step. No workload code is involved.

```rust
use std::cell::Cell;
use moonpool_sim::{Invariant, SIM_FAULT_EVENT_NAME, TraceQuery, assert_always};

struct KillRateInvariant { cursor: Cell<usize>, kills: Cell<usize> }

impl Invariant for KillRateInvariant {
    fn name(&self) -> &str { "kill_rate" }

    fn observe(&self, q: &dyn TraceQuery, _t: u64) {
        for e in q.since(SIM_FAULT_EVENT_NAME, &self.cursor) {
            if e.str("kind") == Some("process_force_kill") {
                self.kills.set(self.kills.get() + 1);
            }
        }
        assert_always!(self.kills.get() <= 10, "too many kills");
    }

    fn reset(&mut self) { self.cursor.set(0); self.kills.set(0); }
}
```

The `kind` values, with their fields:

| Family | `kind` | Fields |
|--------|--------|--------|
| Process | `process_graceful_shutdown` | `ip`, `grace_period_ms` |
| | `process_force_kill` | `ip`, `cause` (`grace_period_expired`, `crash`, `crash_and_wipe`, `restart_in_place`) |
| | `process_restart` | `ip` |
| Network | `partition_created`, `partition_healed` | `from`, `to` |
| | `send_partition_created`, `send_partition_healed`, `recv_partition_created`, `recv_partition_healed` | `ip` |
| | `interface_clogged` | `ip`, `direction` (`send`, `recv`, `both`), `duration_ms` |
| | `random_close` | `connection_id` |
| | `black_hole` | `connection_id`, `direction` (`send`, `recv`, `both`) |
| | `bit_flip` | `connection_id`, `flip_count` |
| Storage | `storage_read_fault`, `storage_sync_fault` | `ip`, `file_id` |
| | `storage_write_fault` | `ip`, `file_id`, `write_kind` (`phantom`, `misdirected`, `corruption`) |
| | `storage_disk_failure`, `storage_crash`, `storage_wipe` | `ip` |

The value is **correlation**. A conservation violation at `t=5000` next to a `process_force_kill` at `t=4980` is a different bug from one with no fault nearby. Both sit in one timeline with one clock, the way an alert window sits next to infrastructure events in a log aggregator.

## Simulation Time in Log Output

`tracing_subscriber::fmt::layer()` prints wall-clock time. In a simulation every line gets the same timestamp. `SimTime` is a `FormatTime` that prints the simulated clock instead, read through the `Clock` trait that `SimulationLayerHandle` implements:

```rust
use moonpool_sim::{SimTime, SimulationLayer};
use tracing_subscriber::layer::SubscriberExt;

let sim_layer = SimulationLayer::new();
let handle = sim_layer.handle();

// SimulationLayer goes first so its clock is current when fmt formats.
let subscriber = tracing_subscriber::registry()
    .with(sim_layer)
    .with(tracing_subscriber::fmt::layer().with_timer(SimTime::new(handle.clone())));

let _guard = tracing::subscriber::set_default(subscriber);
```

```text
sim+    1.234s  INFO raft: term=3 leader=10.0.1.2 leader_elected
```

The orchestrator pushes `handle.set_sim_time_ms(..)` after each step. The handle is per layer, so two simulations in one process keep separate clocks. In production, use the plain wall-clock formatter.

## Snapshots, Timelines, Assertions

Three tools, three jobs.

**`StateHandle::publish` / `get`** holds the latest snapshot. Workloads publish a model with `ctx.state().publish("model", model.clone())` and read it in `check`. Good for "the sum of balances equals deposits minus withdrawals".

**The trace timeline** holds history. Good for "no message was received before it was sent", "the term never decreased", "retries stayed high after the partition healed", which is the metastable-failure signature.

**Assertions** inside a workload or process check the local view.

A rule of thumb: a property that involves more than one process or workload is an invariant. A property about one actor's view is an assertion. Inside an invariant, still use the assertion macros, so a failure carries the seed.

## Performance

Invariants run after **every** step, thousands of times per seed. Use `since` with a cursor, not `snapshot`. Compare counters, check a predicate. The `format!` in a failure message is fine because it runs only on failure. A slow check, such as replaying a log, belongs in `Workload::check` at the end of the seed.
