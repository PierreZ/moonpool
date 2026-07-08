# The Deterministic Executor

<!-- toc -->

Moonpool simulations do not run on tokio. They run on a purpose-built,
single-threaded executor whose every scheduling decision derives from the
iteration seed. This chapter explains why it exists, how it works, and what
changes if you are used to tokio.

## Why not tokio?

Two reasons, one practical and one fundamental.

The practical one: seeding tokio's runtime randomness requires the unstable
`RngSeed` API, which forces `--cfg tokio_unstable` onto every crate in every
consumer's workspace. For stable-pinned projects that is a hard adoption
barrier (issue #151).

The fundamental one: tokio's current-thread scheduler is a FIFO queue, and
that is the *wrong* policy for a bug-finding simulator. When one event makes
two tasks runnable, FIFO runs them in registration order, on every seed,
forever. Any race between those tasks is structurally unexplorable: no
amount of seeds will ever try the other order. A simulation executor should
treat "which runnable task goes next" as a *decision to explore*, the same
way it explores message latencies and fault timings.

## Randomized AND deterministic

Those two properties sound contradictory, but they are not. Deterministic
means "same seed, same execution, bit for bit", not "one fixed schedule".
The executor keeps its ready tasks in a `Vec` and picks the next one with a
`swap_remove` at a seeded-random index. madsim uses the same distribution,
and FoundationDB's sim2 randomizes the same way:

```text
seed 42:  task-3, task-0, task-2, task-1, ...
seed 42:  task-3, task-0, task-2, task-1, ...   (always)
seed 43:  task-1, task-3, task-0, task-2, ...
```

Each seed replays exactly. The *population* of seeds covers the ordering
space. Ordering bugs, the kind that survive code review because both orders
look fine locally, live exactly there.

The scheduling RNG is a private ChaCha8 stream salted from the iteration
seed. It is deliberately not the counted `SIM_RNG` stream: scheduling
decisions never shift the fork explorer's `count@seed` replay bookkeeping.

## Where is the reactor?

There is none, and that is the deep reason this executor is small. A
production runtime pairs its executor with a reactor (epoll, timers) that
turns OS events into waker calls. In the simulation, `SimWorld`'s event
queue plays that role: virtual time, network delivery, and storage
completions are queue entries whose processing wakes the parked task through
its ordinary `Waker`. The executor only answers one question: of the tasks
that are runnable *right now*, which runs next?

## The drive loop

`Executor::block_on(main)` pins the orchestrator future on the stack as the
*driver*. It never enters the ready queue and is never scheduled randomly:

```text
loop {
    poll driver ── Ready? ─────────────► return
    run_until_stalled()   // poll ready tasks in seeded-random order
                          // until none is runnable
    (driver not woken AND queue empty) ► panic: deadlock (with the seed)
}
```

The orchestrator's step loops interleave the simulation with the task pool:

```rust,ignore
sim.step();                              // process one virtual event
crate::executor::until_stalled().await;  // let every woken task run
```

Because the driver is only re-polled after a complete drain, resuming from
`until_stalled().await` guarantees that every task that was runnable has
been polled to `Pending` or completion. Under tokio this guarantee was
approximated by `yield_now` plus FIFO ordering. Under randomized scheduling
"the back of the queue" does not exist, so the executor provides the
contract directly.

Inside a task, use `executor::yield_now()` instead: it reschedules the task
at a seeded-random position. `until_stalled()` is driver-only and asserts
it, in every build profile, because a task awaiting "run until nothing is
runnable" would be waiting for itself.

One consequence deserves a warning sign: a task must not busy-wait on
progress only the driver can make. A `loop { yield_now().await }` polling
for a flag that a simulation event sets keeps the ready queue non-empty
forever, so the drain never finishes and `sim.step()` never runs. The
executor kills such a loop with a poll-bound panic naming the task and the
seed. Park on a real wake instead: a sleep, a `Notify`, a channel.

## Coming from tokio

The task API mirrors what the orchestrator (and your process/workload code,
through `TaskProvider`) already expected from tokio:

| tokio | moonpool executor |
|---|---|
| `tokio::spawn(fut)` | `executor::spawn("name", fut)` (named, for seed debugging) |
| `JoinHandle::is_finished()` | same |
| `JoinHandle::abort()` | same (await yields `Err(JoinError::Cancelled)`) |
| dropping a `JoinHandle` | same: detaches, task keeps running |
| task panics | caught, handle yields `Err(JoinError::Panicked)`, siblings unaffected |
| dropping the `Runtime` | dropping the `Executor` cancels every live task |
| `tokio::select!` | `moonpool::select!` (seeded rotation, see the select chapter) |

Spawned futures stay `Send + 'static`, exactly as before: execution is one
OS thread, but the bounds let application code use `Arc<RwLock<...>>`,
`DashMap`, and friends without contortion.

Two behaviors are deliberately **better** than tokio's:

- A genuine deadlock (driver not woken, nothing runnable) panics with the
  seed in the message instead of parking the thread forever.
- A task that stays runnable forever, whether a busy-yield loop or a select
  over a source it synchronously re-readies (something tokio's cooperative
  budget used to interrupt), trips a poll-bound panic naming the task and
  the seed, in every build profile, instead of hanging the drain loop.

## Kill-on-drop

Simulation iterations must be hermetic: a task leaked by seed N must never
run during seed N+1. Dropping the per-iteration tokio runtime used to
guarantee that, and the executor replicates it with a waker registry (the
same pattern `async-executor` uses). Every live task registers one waker.
`Drop` wakes them all, which schedules every parked task, then pops and
drops `Runnable`s until the queue is empty. Dropping a `Runnable` cancels
the task and drops its future, and any tasks that drop wakes land in the
same queue and are consumed by the same loop.

## Fork safety

The multiverse explorer forks the process *while tasks are being polled*
(assertion macros are fork points). Two rules keep that sound: everything is
single-threaded, and the ready-queue lock is never held across a task poll,
so a forked child never inherits a held lock.

## Verifying it yourself

The executor's contracts are enforced by `moonpool-sim/tests/executor.rs`
(join/abort/detach/panic/kill-on-drop, same-seed replay, cross-seed
diversity) and `moonpool-sim/tests/determinism.rs`, an end-to-end tripwire:
two full `SimulationBuilder` runs of racing workloads on the same seed must
produce byte-identical execution traces. If any component, executor,
`select!`, providers, ever consults an unseeded randomness source, that test
fails.
