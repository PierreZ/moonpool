# Reading the Event Trace

<!-- toc -->

You have a pinned seed reproducing the failure. Now you need to understand what happened. The event trace is your primary tool.

## The Scheduler

Moonpool's simulation engine is built around `Scheduler<Event>`, a priority
scheduler ordered by logical time and a stable `ScheduleId`. It owns the one
monotonic simulation clock, same-time FIFO sequence allocation, and eager
cancellation. Scheduling into the past clamps to the current time. Cancelled
entries disappear without advancing time.

The scheduler coordinates delayed work without owning resource state. It
dispatches `NetworkEvent` values to `NetworkSimulation` and `StorageEvent`
values to `StorageEngine`. Those components update their own state, resolve the
exact pending operation, and return ordered effects, faults, and wake batches.
The coordinator applies those effects and wakes tasks after releasing the world
lock.

When you enable trace-level logging (`RUST_LOG=trace`), you see every event as it fires:

```
  Processing event at t=1.234s seq=47: Network(DataDelivery { connection_id: 3, ... })
  Processing event at t=1.234s seq=48: Timer { task_id: 12 }
  Processing event at t=2.500s seq=49: Storage(operation_id=9, WriteComplete)
```

Each line tells you what happened, when, and in what order.

## Key Event Types

The simulation has a small set of event types, and learning to recognize them makes traces much easier to read.

**Timer** events wake sleeping tasks. When your workload calls `time.sleep(Duration::from_secs(1))`, that schedules a Timer event one second in the future. These are the heartbeat of your simulation.

**Network** events target the network engine. `OperationReady` completes one
delayed bind, connect, or accept latency. `DataDelivery` puts bytes into a
connection's receive buffer. `ProcessSendBuffer` drains the send side.
`FinDelivery` signals a graceful close after all data has been delivered.

**Network maintenance** events change connection-wide state. `PartitionRestore`,
`SendPartitionClear`, and `RecvPartitionClear` remove expired cuts. `ClogClear`
and `ReadClogClear` release parked stream operations only when the event still
matches the active deadline, so an old expiry cannot clear a newer clog.

**Storage** events carry an exact `OperationId`, handle ID, and operation kind.
Reads, writes, syncs, and set-length operations complete only their matching
pending entry. The engine stores an explicit success or error result and wakes
only that operation's waiter.

**Process lifecycle** events manage reboots. `ProcessGracefulShutdown` signals a process to clean up. `ProcessForceKill` aborts it after the grace period. `ProcessRestart` brings it back.

**Shutdown** wakes all tasks for orderly termination at the end of a simulation.

## Tracing the Causal Chain

When an assertion fires, the question is: **what caused this?** The event trace gives you the answer, but you read it backwards.

Start at the failure. Look at the last few events before the assertion. Usually
one of them is the trigger: a `DataDelivery` that delivered a stale message, a
`Timer` that expired causing a timeout, or an `OperationReady` that let a
connection race complete. Then ask which component requested that schedule.
Follow the chain back through the trace.

For example, suppose your conservation law invariant fires after event #312. Look at event #312: it is a `DataDelivery` on connection 7. What was connection 7? The trace shows it was established at event #201 between the workload and a KV server process. What did the delivery contain? A withdraw response. But the model expected a deposit. Now you have a lead.

## Using RNG Call Count

Every random decision in the simulation consumes one or more calls to the deterministic RNG. The total call count at any point in the execution is a precise fingerprint of "where we are."

When comparing a working seed against a failing seed, the RNG call count tells you exactly where their executions diverge. If both seeds process events identically through RNG call 847, but diverge at call 848, the code executing at that point made a different random choice that led down the failing path.

This technique is especially useful for regression testing: if you fix a bug and the RNG call pattern changes, you know your fix altered the execution path (which is expected). If it does not change, your fix might not be reaching the right code.

## Infrastructure vs Workload Events

Not every event in the trace matters to your investigation. The simulation
marks some events as **infrastructure**: `PartitionRestore`,
`SendPartitionClear`, `RecvPartitionClear`, and `ProcessRestart`. These maintain
simulation state but do not represent application work.

The simulation uses this distinction internally to decide when to terminate.
After all workloads finish, if only infrastructure events remain in the
scheduler, the simulation can safely end. When reading traces, you can often
skip them and focus on `OperationReady`, `DataDelivery`, `Timer`, and `Storage`
events that directly affect application progress.

## Practical Tips

**Start narrow**. Use `RUST_LOG=error` first to see just the failure. Then widen to `RUST_LOG=debug` or `RUST_LOG=trace` only if you need more context.

**Search for the event sequence number**. The invariant failure happens after a specific `sim.step()` call. The event processed in that step has a sequence number. Search for it in the trace.

**Count backwards**. If the failure is at event #312, the cause is often in the 5-10 events before it, not 200 events earlier.

**Compare two seeds**. Run a passing seed and a failing seed side by side with trace output. Diff the two traces. The first divergence point is where the bug's path begins.
