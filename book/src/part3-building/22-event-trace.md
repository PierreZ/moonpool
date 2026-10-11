# Reading the Event Trace

<!-- toc -->

You have a pinned seed that reproduces the failure. Now you need to know what happened. The event trace is the primary tool, and reading it is a skill: know the event kinds, know who produces them, and read backwards.

## One Scheduler, One Clock

The simulation runs on a single `Scheduler<Event>`, a priority queue ordered by logical time and then by a stable sequence number. It alone owns the clock. Two events at the same time are processed in FIFO order of their sequence numbers. Scheduling into the past clamps to now. A cancelled entry disappears without advancing time.

The scheduler coordinates delayed work without owning resource state. It hands `NetworkEvent`s to the `NetworkSimulation` and `StorageEvent`s to the `StorageEngine`. Each component updates its own state, resolves the exact pending operation, and returns ordered effects, fault records and wake batches. The world applies the effects and wakes tasks only after it releases its lock. That ownership split is why every event names exactly one operation, and why a trace line is enough to find the pending future it completed.

## The Event Kinds

| Event | Meaning | Produced by |
|---|---|---|
| `Timer { task_id }` | A sleep or timeout elapsed. The heartbeat of every workload. | `sim/sleep.rs` through the scheduler |
| `Network(OperationReady)` | A delayed bind, connect or accept completed its latency. | `network/sim/engine.rs` |
| `Network(ProcessSendBuffer)` | The next chunk of a connection's send queue goes onto the wire. | `network/sim/delivery.rs` |
| `Network(Delivery { connection_id, seq })` | An in-flight item (data or FIN) reached its delivery time. A wake-up, not a payload: the item still sits in the sender's flight, so a partition injected after scheduling still decides its fate. | `network/sim/delivery.rs` |
| `Network(PartitionRestore / SendPartitionClear / RecvPartitionClear)` | An expired cut is removed and held items are re-timed. | `network/sim/faults.rs` |
| `Network(ClogClear / ReadClogClear)` | A clog expires, only if the event still matches the active deadline. | `network/sim/faults.rs` |
| `Network(Maintenance)` | A wake-up to evaluate network maintenance at this time. | `network/sim/engine.rs` |
| `Storage(StorageEvent)` | One exact read, write, sync or set-length completes, by operation id and handle id. | `storage/sim/complete.rs`, scheduled in `storage/sim/engine.rs` |
| `ProcessGracefulShutdown { ip, .. }` | A graceful reboot begins: the token fires, the grace period starts. | `runner/process_manager.rs` |
| `ProcessForceKill { ip, cause, .. }` | The process task is aborted. `cause` is `GracePeriodExpired`, `Crash`, `CrashAndWipe` or `RestartInPlace`. | `runner/process_manager.rs`, `SimContext::crash_self` |
| `ProcessRestart { ip }` | A rebooted process boots again from its factory. | `runner/process_manager.rs` |
| `CpuFree { ip }`, `CpuWake { id }` | The CPU model released a core or a deferred wake. Only with `Chaos::Cpu`. | `sim/cpu.rs`, see [CPU Time](./26-cpu-and-gray-failure.md) |
| `Shutdown` | Wake every task for orderly termination. | the orchestrator |

Every storage operation is `Pending` first and completes through its own event. Every network bind, connect and accept is a delayed operation too. Only established stream I/O can complete on the current poll, when bytes or capacity are already there.

## Faults in the Timeline

Every fault the engines inject is also an event in the captured timeline, under the name `sim_fault` with a `kind` field. The runner merges `SimWorld::take_faults()` into the timeline after each step, with `source = "sim"`. The kinds mirror the fault surfaces: `partition_created`, `partition_healed`, `interface_clogged`, `random_close`, `black_hole`, `bit_flip`, `storage_read_fault`, `storage_write_fault`, `storage_sync_fault`, `storage_disk_failure`, `storage_crash`, `storage_wipe`, `process_graceful_shutdown`, `process_force_kill`, `process_restart` and the directed partition pairs.

A workload reads them in `check()` through `ctx.observability()`, and an invariant reads them with a cursor:

```rust
use moonpool_sim::{SIM_FAULT_EVENT_NAME, TraceQuery};

for e in q.since(SIM_FAULT_EVENT_NAME, &self.cursor) {
    if e.str("kind") == Some("process_force_kill") {
        // the process at e.str("ip") just died
    }
}
```

This is often the fastest route to a cause: the fault that preceded the violation is a named row with a simulated timestamp, not a line to infer from symptoms.

## Reading Backwards

When an assertion fails, the question is **what caused this**. The trace answers it, read from the failure toward the start.

1. Find the **first** `[ASSERTION FAILED]` line. Later ones are usually cascade from the same cause.
2. Look at the handful of events before it. One of them is the trigger: a `Delivery` that landed a stale message, a `Timer` that fired a timeout, an `OperationReady` that let a connect race complete, a `ProcessForceKill` that dropped in-memory state.
3. Ask which component requested that schedule, and follow the chain back.

Suppose the conservation invariant fires at simulated time 12.4s. The previous `sim_fault` row is a `process_force_kill` of `10.0.1.2` at 12.1s with cause `crash`. The `Delivery` just before the violation is on a connection the workload opened to `10.0.1.2` at 11.9s. The server answered a get from an empty map because the crash emptied it. Now you have the cause, and it is in the server, not the workload.

Same-time events are FIFO by sequence number. If two completions seem to have swapped order between a passing and a failing seed, look for a changed `schedule_at` or `schedule_after` call, or a draw inserted before it.

## The RNG Call Count

Every random decision consumes one or more draws on the single simulation stream. `moonpool_sim::rng_call_count()` at any point is a precise fingerprint of "where we are". Two runs of one seed that agree through draw 847 and differ at draw 848 diverged in the code that made draw 848. [Reproducing with FixedCount](./21-reproducing.md#when-the-bug-disappears) uses this with `set_rng_breakpoints` to bisect non-determinism, and exploration uses the same count to anchor its recipes.

## Infrastructure vs Application Events

Not every event matters to an investigation. `PartitionRestore`, `SendPartitionClear`, `RecvPartitionClear` and `ProcessRestart` are **infrastructure** events: they maintain simulation state and do no application work. The runner uses this distinction to decide when a seed may end once every workload has returned. When reading a trace, skip them and focus on `Delivery`, `OperationReady`, `Timer`, `Storage` and the process kills.

## Practical Tips

**Start narrow.** Run the pinned seed at the default level first, read the `[ASSERTION FAILED]` lines and the `sim_fault` rows. Widen to `trace_level(LevelFilter::TRACE)` only when the rows do not explain it.

**Print simulated time.** `SimTime` as the subscriber's timer, as [Events and Invariants](./17-events-and-invariants.md#simulation-time-in-log-output) shows. Wall-clock timestamps on a simulated run are all the same millisecond.

**Count backwards.** The cause is usually within the ten events before the failure, not two hundred earlier.

**Compare two seeds.** Run a passing seed and the failing seed at the same level and diff the logs. The first divergence is where the failing path begins.

**Emit your own events.** A `tracing::info!(key = %key, "kv_set_committed")` in the workload is both the invariant's input and your breadcrumb in the trace.
