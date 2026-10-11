# The Deterministic Executor

<!-- toc -->

Moonpool simulations do not run on tokio. They run on `moonpool_sim::executor::Executor`, a single-threaded executor whose every scheduling decision derives from the iteration seed. This chapter explains why it exists, how it works, and what changes if you come from tokio.

## Why Not tokio?

Two reasons, one practical and one fundamental.

The practical one: seeding tokio's runtime randomness needs the unstable `RngSeed` API, which forces `--cfg tokio_unstable` onto every crate in every consumer's workspace. For a project pinned to stable Rust that is a hard adoption barrier.

The fundamental one: tokio's current-thread scheduler is a FIFO queue, and FIFO is the **wrong** policy for a bug-finding simulator. When one event makes two tasks runnable, FIFO runs them in registration order, on every seed, forever. Any race between those two tasks is structurally unexplorable. No number of seeds will ever try the other order. A simulation executor must treat "which runnable task goes next" as a **decision to explore**, like a message latency or a fault timing.

## Randomized and Deterministic

These two properties sound contradictory. They are not. Deterministic means "same seed, same execution, bit for bit", not "one fixed schedule". The executor keeps its ready tasks in a `Vec` and picks the next one with `swap_remove` at a seeded-random index. madsim uses the same distribution, and FoundationDB's sim2 randomizes the same way:

```text
seed 42:  task-3, task-0, task-2, task-1, ...
seed 42:  task-3, task-0, task-2, task-1, ...   (always)
seed 43:  task-1, task-3, task-0, task-2, ...
```

Each seed replays exactly. The **population** of seeds covers the ordering space. Ordering bugs, the kind that survive code review because both orders look fine, live exactly there.

The scheduling index is a draw on the simulation's one random stream, the same `sim_random_range` a process reaches through `ctx.random()`. There is no private scheduling RNG. Which task runs next is counted like every other decision, so an exploration recipe replays the schedule along with the faults. The executor draws nothing when exactly one task is runnable: that is a forced choice, not a decision, and the dominant one-event-wakes-one-task path leaves the stream untouched.

## Where Is the Reactor?

There is none. A production runtime pairs its executor with a reactor (epoll, a timer wheel) that turns OS events into waker calls. In the simulation, `SimWorld` and its `Scheduler<Event>` play that role. The scheduler owns logical time, stable same-time ordering and cancellation. It dispatches events to `NetworkSimulation` and `StorageEngine`, which own their resource state and their waker registries. The wake batches they return are invoked after the world lock is released. The executor answers one question only: of the tasks runnable **right now**, which one runs next?

## The Drive Loop

`Executor::block_on(main)` pins the orchestrator future on the stack as the **driver**. It never enters the ready queue and is never scheduled randomly:

```text
loop {
    poll driver ── Ready? ─────────────► return
    run_until_stalled()   // poll ready tasks in seeded-random order
                          // until none is runnable
    (driver not woken AND queue empty) ► panic: deadlock (with the seed)
}
```

The orchestrator's step loop interleaves the world with the task pool:

```rust,ignore
sim.step();                              // process one virtual event
crate::executor::until_stalled().await;  // let every woken task run
```

The driver is re-polled only after a complete drain. So when `until_stalled().await` returns, every task that was runnable has been polled to `Pending` or to completion. Under tokio this was approximated with `yield_now` and FIFO ordering. Under randomized scheduling "the back of the queue" does not exist, so the executor provides the contract directly.

Inside a task, use `executor::yield_now()` instead. It reschedules the task at a seeded-random position. `until_stalled()` is driver-only and asserts it in every build profile: a task that awaits "run until nothing is runnable" waits for itself.

One consequence deserves a warning sign. A task must not busy-wait on progress that only the driver can make. A `loop { yield_now().await }` that polls a flag set by a simulation event keeps the ready queue non-empty forever, so the drain never finishes and `sim.step()` never runs. The executor kills such a loop after 1,000,000 polls in one drain, with a panic that names the task and the seed. Park on a real wake instead: a sleep, a `Notify`, a channel.

## Task Owners and the CPU Gate

Every task carries metadata: an id, a name, and an **owner**. The owner is the IP of the simulated process whose root task spawned it, inherited by every task spawned while one of its tasks polls. The driver, the workloads and the fault injectors have no owner.

The owner serves two features. A `hint!("label").await` in shipped code finds the process to reboot through the owner of the task being polled, without being handed one. And with the CPU model on (`Chaos::Cpu`), a process's task polls only when its one CPU slot is free. Otherwise the world parks the runnable and schedules a `CpuFree` event. A parked task is not runnable, so `until_stalled` keeps its contract. Tasks without an owner never pay. [CPU Time and Gray Failures](../part3-building/26-cpu-and-gray-failure.md) covers the model.

## Coming from tokio

The task API mirrors what the orchestrator, and your process or workload code through `TaskProvider`, expected from tokio:

| tokio | moonpool executor |
|---|---|
| `tokio::spawn(fut)` | `executor::spawn("name", fut)` (named, for seed debugging) |
| `JoinHandle::is_finished()` | same |
| `JoinHandle::abort()` | same (await yields `Err(JoinError::Cancelled)`) |
| dropping a `JoinHandle` | same: detaches, the task keeps running |
| fire-and-forget | `JoinHandle::detach()`, the explicit form of the above |
| task panics | caught, the handle yields `Err(JoinError::Panicked)`, siblings unaffected |
| dropping the `Runtime` | dropping the `Executor` cancels every live task |
| `tokio::select!` | `moonpool_sim::select!` (tokio's own expansion, seeded start offset) |

Spawned futures stay `Send + 'static`. Execution is one OS thread, but the bounds let application code use `Arc<RwLock<…>>` and `DashMap` without contortion.

Panic handling has two layers. A task spawned with `executor::spawn` records its panic in its own `JoinHandle`. A task spawned through `ctx.task().spawn_task()` carries one more contract: the runner records an unobserved child panic with its process or workload identity, as an `unobserved_task_panic` event, and marks the seed as failed. If the caller awaits the handle and handles `Err(JoinError::Panicked)`, the panic is observed and the seed can recover. Dropping or detaching the handle leaves the panic unobserved. A process's tasks are also bound to the **scope** of its boot: when the process is killed, every task it spawned, however deep, is dropped with it. That drop is cancellation, not a panic.

`time.sleep(Duration::ZERO)` schedules a real same-time timer, so a burst of immediate work keeps the scheduler's FIFO order. The runner allows the fan-out, then requests shutdown after 100,000 consecutive events at one logical instant. If the tasks ignore shutdown, the seed fails as a deadlock. Time advancing resets the count, so long timer-driven runs keep working.

Two behaviours are deliberately **better** than tokio's:

- A genuine deadlock (driver not woken, nothing runnable) panics with the seed in the message instead of parking the thread forever.
- A task that stays runnable forever, a busy-yield loop or a `select!` over a source it re-readies synchronously, trips the poll bound in every build profile instead of hanging the drain.

## Kill-on-Drop

Simulation iterations must be hermetic: a task leaked by seed N must never run during seed N+1. Dropping the per-iteration tokio runtime used to guarantee that. The executor replicates it with a waker registry, the pattern `async-executor` uses. Every live task registers one waker. `Drop` wakes them all in task-id order, which schedules every parked task, then pops and drops `Runnable`s until the queue is empty. Runnables the CPU model parked are dropped too. Dropping a `Runnable` cancels its task and drops its future. Any task that drop wakes lands in the same queue and the same loop consumes it.

## Exploration Process Safety

The explorer forks only between complete timelines, after the executor and all of its tasks have been dropped. Assertion macros record discovery coordinates. They are not fork points. Forked workers are still opt-in, because unrelated host threads may hold locks that the child inherits. The default `workers: 0` mode runs continuations in-process and is the right choice for CI.

## Verifying It Yourself

`crates/moonpool-sim/tests/executor.rs` enforces the contracts: join, abort, detach, panic isolation, kill-on-drop, same-seed replay and cross-seed diversity. `crates/moonpool-sim/tests/determinism.rs` is the end-to-end tripwire: two full `SimulationBuilder` runs of racing workloads on the same seed must produce byte-identical traces. If any component, the executor, `select!` or a provider, ever consults an unseeded source, that test fails.
