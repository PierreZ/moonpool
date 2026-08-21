# Bounded Workers

<!-- toc -->

The previous explorer *was* its process tree: a discovery forked the running simulation mid-poll, children forked again at their own discoveries, and the logical exploration tree materialized as a recursive tree of live OS processes. It worked — and it made deep exploration impractical, because depth multiplied the number of simultaneously live processes.

The redesign inverts the relationship:

> The size of the logical exploration space is independent of the number of simultaneously active OS processes.

## fork() Is an Optimization, Not a Data Structure

The controller executes each job through a fixed pool of workers:

```text
controller ──fork──▶ worker (slot 0) ── run ONE timeline ──▶ _exit
           ──fork──▶ worker (slot 1) ── run ONE timeline ──▶ _exit
           ◀─waitpid── read journal from the slot, decide, expand
```

A worker is forked at a *quiescent point* between runs — never mid-simulation — inherits the whole prepared runner state via copy-on-write, executes exactly one replay-plus-continuation, serializes its discovery journal (and sanitizer-coverage counters) into its `MAP_SHARED` result slot, and `_exit`s with 0 (clean), 42 (simulation failure), or anything else (crash). Workers never fork, never touch the frontier, and never return into controller code — a panic inside a worker is caught and converted into an exit code, so recursion is structurally impossible.

Live processes are therefore bounded by `1 + workers` at all times. Reaching a state fifty discoveries deep costs a fifty-segment recipe, not fifty live processes.

With `workers: 0` the controller runs every job in-process: sequential, fork-free, and fully deterministic — the same exploration schedule every time. This is the mode to use when debugging the explorer itself (and the only mode on non-unix targets). With `workers > 0` each timeline is still individually reproducible from its recipe, but the *search order* depends on which worker finishes first.

## What Crosses the Process Boundary

Exactly three things are shared between controller and workers:

1. **The assertion region** (`MAP_SHARED`): pass/fail counts accumulate across processes, and the discovery CAS latches make novelty a race-free, exactly-once decision.
2. **Result slots**: one per worker, carrying the run's discovery journal out of the worker before it exits. A crashed worker leaves an empty journal; its recipe is still retained as a crash reproducer.
3. **Sancov buffers**: LLVM `inline-8bit-counters` are copied into a per-worker slot and merged into the controller-owned history map after `waitpid` — a single owner, no concurrent merges.

Everything else — frontier, exemplars, statistics, bug recipes — lives in ordinary controller memory with exactly one owner.

## Portability

The explorer uses only portable POSIX primitives: `fork`, `waitpid` (with `EINTR` retry), and `mmap(MAP_SHARED | MAP_ANONYMOUS)`. There is no `clone3`, no pidfd, no `/proc`, no cgroups — the same code runs on macOS and Linux, and CI exercises both. Fork safety holds because the whole simulation stack is single-threaded by design: the deterministic executor runs on one OS thread, and the controller forks only between runs when no locks are held.
