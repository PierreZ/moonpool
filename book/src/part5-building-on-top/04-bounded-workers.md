# Bounded Workers

<!-- toc -->

An earlier explorer **was** its process tree. A discovery forked the running simulation mid-poll. Children forked again at their own discoveries. The logical tree became a tree of live OS processes. It worked, and it made deep exploration impractical: depth multiplied the number of live processes.

The current design inverts the relationship:

> The size of the logical exploration space is independent of the number of live OS processes.

## fork() Is an Optimization, Not a Data Structure

The controller runs each job through a fixed pool of workers:

```text
controller --fork--> worker (slot 0) -- runs ONE timeline --> _exit
           --fork--> worker (slot 1) -- runs ONE timeline --> _exit
           <-waitpid- read the slot's journal, decide, expand
```

The controller forks at a **quiescent point** between runs, never mid-simulation. The child inherits the prepared runner state by copy-on-write. It runs exactly one replay plus continuation, serializes its discovery journal and sancov counters into its `MAP_SHARED` result slot, and calls `_exit`. The exit code says how the run ended:

| Exit | Meaning |
|------|---------|
| 0 | The run completed and passed. |
| 42 | The run completed and reported a simulation failure. |
| any other code, or a signal | The worker crashed (a panic, an abort). |

Nothing catches a panic inside a worker. The worker dies, the controller classifies the `waitpid` status, and the recipe is kept as a crash reproducer like any other failure. Workers never fork, never touch the frontier, and never return into controller code. Recursion is structurally impossible.

Live processes are bounded by `1 + workers` at all times. A state fifty discoveries deep costs a fifty-segment recipe, not fifty processes. If `fork` itself fails (a resource limit), the controller runs that job in-process instead.

With `workers: 0` the controller runs every job in-process: sequential, fork-free, and fully deterministic. The same inputs give the same statistics and the same bug recipes every time, and the integration test `in_process_exploration_is_deterministic` checks exactly that. Use this mode in `cargo nextest` tests and on non-unix targets, where it is the only mode. With `workers > 0`, every timeline is still reproducible from its recipe, but the **search order** depends on which worker finishes first.

## The Wall-Clock Ceiling

A worker stuck outside the simulation (a tight loop in user code, a blocking syscall) is invisible to the in-simulation stall guards. The controller stamps each fork with the wall clock. A worker still running after `DEFAULT_WORKER_TIMEOUT` (5 minutes) is killed with `SIGKILL`, reaped, and recorded as a crash with its recipe printed. `Explorer::set_worker_timeout` changes the ceiling. A timeline normally takes milliseconds to seconds, so the default is generous on purpose.

The controller polls its own children by pid with `WNOHANG`, never `waitpid(-1)`, so a harness that embeds the explorer keeps its own children. It spins for 2048 polls, then sleeps between 10 microseconds and 1 millisecond, so short timelines pay no sleep latency. Dropping an `Explorer` kills and reaps every live worker, so a panic in the harness cannot orphan a worker that holds the shared regions.

## What Crosses the Process Boundary

Exactly three things are shared between controller and workers:

1. **The assertion region**: the 2048-slot assertion table and the 256-bucket `assert_sometimes_each!` table from `moonpool-assertions`, mapped `MAP_SHARED | MAP_ANONYMOUS` by `init_assertions`. Pass and fail counts accumulate across processes. The discovery latches make novelty a race-free, exactly-once decision.
2. **Result slots**: one per worker. A slot holds a `u32` length and up to 256 raw events of `(call_count, state_id, kind)`. The controller zeroes the slot before the fork. The worker writes the entries first and the length last, so an interrupted worker leaves an empty journal. Its recipe is still kept as a crash reproducer.
3. **Sancov buffers**: LLVM `inline-8bit-counters` copied into a per-worker slot and merged into the controller-owned history map after `waitpid`. One owner, no concurrent merges. See [Running Simulation Binaries with xtask](../appendix/08-xtask.md) for how the counters get into the binary.

Everything else (frontier, exemplars, statistics, bug recipes) lives in ordinary controller memory with one owner.

Under exploration the runner scrapes application metrics from root timelines only. Explored timelines report back through the shared assertion table, which carries no metric values.

## Portability

The explorer uses only portable POSIX primitives: `fork`, `waitpid` with `EINTR` retry, `kill`, and `mmap(MAP_SHARED | MAP_ANONYMOUS)`. There is no `clone3`, no pidfd, no `/proc`, no cgroups. The same code runs on macOS and Linux, and CI runs the `sim-frontier-explore` binary on both. Fork safety holds because the simulation stack is single-threaded by design: the deterministic executor runs on one OS thread, and the controller forks only between runs when no lock is held.
