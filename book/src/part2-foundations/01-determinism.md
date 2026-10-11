# Determinism as a Foundation

<!-- toc -->

A distributed system dropped writes for 47 seconds. The logs show three nodes that disagreed about leadership. The window has passed. The cluster healed itself. You cannot reproduce the failure on your laptop. You cannot reproduce it in staging. You spend three days in the logs, you form a theory, you ship a fix you are 60% sure about, and you wait for it to happen again.

This is debugging without determinism. The bug is real, but the conditions that caused it are gone. You cannot replay the execution. You cannot step through it. You cannot even confirm that your fix addresses the right cause.

## The Three Enemies

A Rust program that uses async networking has three sources of non-determinism: **thread scheduling**, **real I/O** and **random numbers**.

**Thread scheduling.** The OS decides when a thread runs. Two threads that race on a shared counter increment it correctly 999,999 times, then corrupt it once. The interleaving that triggers the bug depends on CPU load, on thermal throttling, on how many browser tabs are open. A multi-threaded tokio runtime moves tasks across a thread pool. The order in which tasks resume after an `.await` belongs to the scheduler. Run the program twice and you get two execution orders.

**Real I/O.** The outside world injects randomness. A TCP packet arrives 2 ms late. A DNS lookup takes 800 ms instead of 5 ms. A disk write returns `ENOSPC` because another process filled the partition. Your code is a deterministic function, but its inputs are chaos.

**Random numbers.** `rand::rng()` pulls entropy from the OS. Two runs with identical inputs make different random choices: different retry delays, different leader elections, different shard assignments.

A distributed-systems bug usually needs all three at once: a specific interleaving, during a specific network delay, with a specific disk latency. To reproduce the bug you must reproduce all three. That is close to impossible.

## Moonpool's Answer

Moonpool brings all three sources under one seed.

**One thread, one executor.** A simulation runs on `moonpool_sim::executor::Executor`, a single-threaded executor that moonpool owns. There is no tokio runtime inside a simulation. When several tasks are ready, the executor picks the next one with a seeded draw. [The Single-Core Constraint](./02-single-core.md) and [The Deterministic Executor](./11-executor.md) explain this.

**Providers instead of raw I/O.** Every system call your code makes goes through a trait: `TimeProvider`, `TaskProvider`, `NetworkProvider`, `RandomProvider`, `StorageProvider` and `SystemProvider`. In production the trait calls tokio. In simulation the trait calls the simulated world. The code in between is identical. [The Provider Pattern](./04-provider-pattern.md) covers this.

**One random stream.** The simulation has exactly one `ChaCha8Rng`, seeded from a `u64`. The executor's scheduling draws, the `select!` branch offsets, the network and storage fault coins, buggify activation, the per-seed swarm masks and every `RandomProvider` call your code makes come from that one stream. Moonpool keeps no private RNG for its own decisions. [Seed-Driven Reproducibility](./03-seeds.md) covers this.

With all three under control, a simulation is a **pure function of its seed**. The same seed gives the same execution, bit for bit. A different seed explores a different interleaving, a different fault schedule, a different set of random choices.

## Where Time Lives

The event side of the guarantee is explicit. One `Scheduler<Event>` owns logical time, cancellation and a monotonic sequence number. Events with the same timestamp run in sequence order, so same-time events are FIFO without any dependence on heap internals. Scheduling into the past clamps to the current time.

The scheduler does not own the simulated resources. `NetworkSimulation` owns connections, topology, network faults and network wakers. `StorageEngine` owns files, open handles, disk episodes and storage wakers. Each component consumes its own events and returns ordered effects for the coordinator, `SimWorld`, to schedule. Random decisions happen inside the component that owns the state. Global ordering happens in one scheduler. Wakers run only after the world lock is released. This split is what makes replay exact: there is one place where "what happens next" is decided.

## What This Gives You

A failing seed turns a production incident into a local debugging session. Instead of "it happened once between nodes 7 and 12 under load", the report ends with:

```text
  Faulty seeds: [8839214571]
```

You pin that seed with `set_debug_seeds(vec![8839214571])`, run one iteration, and set a breakpoint. The bug is there, every time. You fix it. You run 10,000 more seeds. They pass.

This is not aspirational. FoundationDB ran 5 to 10 million simulation iterations per night, each with a different seed. Their physical validation cluster, real servers with programmable power switches, never found a database bug that simulation had missed. Determinism made that possible.

Everything in moonpool builds on this foundation. Chaos, assertion coverage, swarm testing, multiverse exploration: all of it rests on one guarantee. Given the same seed, the simulation produces the same result. The rest of this part explains how moonpool keeps that guarantee.
