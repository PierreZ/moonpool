# Determinism as a Foundation

<!-- toc -->

A distributed system dropped writes for 47 seconds. Logs show three nodes disagreed about leadership, but the window where it happened has already passed. The cluster self-healed. You cannot reproduce it locally. You cannot reproduce it in staging. You spend three days staring at logs, form a theory, ship a fix you are 60% sure about, and wait. Until it happens again.

This is what debugging looks like without determinism. The bug is real, but the conditions that triggered it are gone. You cannot replay the execution. You cannot step through it. You cannot even confirm your fix addresses the right cause.

## The Two Enemies

Rust programs that use async networking have three sources of non-determinism: **thread scheduling**, **real I/O**, and **random number generation**.

**Thread scheduling** means the OS decides when threads run. Two threads racing on a shared counter might increment it correctly 999,999 times, then corrupt it once. The interleaving that triggers the bug depends on CPU load, thermal throttling, how many browser tabs you have open. A multi-threaded tokio runtime executes tasks across a thread pool. The order tasks resume after `.await` is up to the scheduler. Run the same program twice, get two different execution orders.

**Real I/O** means the outside world injects randomness. A TCP packet arrives 2ms late. A DNS lookup takes 800ms instead of 5ms. A disk write returns `ENOSPC` because another process filled the partition. Network calls complete in unpredictable order. Timeouts race against responses. Your code is a deterministic function, but its inputs are chaos.

**Random number generation** introduces a subtler form of non-determinism. Calling `rand::rng()` pulls entropy from the OS. Two runs with identical inputs can make different random choices, leading to different retry delays, different leader elections, different shard assignments.

Together, these forces make distributed systems bugs the hardest kind to find. The bug only appears under a specific thread interleaving, during a specific network delay pattern, with a specific disk latency. Reproducing it means reproducing all three. Which is effectively impossible.

## Moonpool's Answer

Moonpool eliminates both sources. Completely.

**Single-core execution** removes thread scheduling from the equation. One thread. One execution order. No races, no interleavings, no "works on my machine but fails in CI." We cover this in detail in the next chapter.

**Provider abstraction** replaces real I/O and real randomness with simulated equivalents. Every system call your code makes (network, disk, time, random numbers) goes through a trait. In production, the trait calls tokio. In simulation, the trait calls moonpool's deterministic runtime. Same code, different wiring. No `#[cfg(test)]` branching. The production code path **is** the tested code path.

With both sources eliminated, the simulation becomes a **pure function** of its seed. A single `u64` value determines everything: which connections fail, when timeouts fire, what order messages arrive, whether disk writes corrupt. Same seed, same execution, same bugs. Every time.

## What This Gives You

A failing seed turns a production incident into a local debugging session. Instead of "it happened once between nodes 7 and 12 under load," you get:

```
FAILED seed=8839214571 — leadership invariant violated at sim_time=12.4s
```

You paste that seed into your test, run it, hit a breakpoint. The bug is right there. You fix it. You run 10,000 more seeds. They pass. You ship with confidence.

This is not aspirational. FoundationDB ran 5 to 10 million simulation iterations per night, each with a different seed. Their physical validation cluster (real servers with programmable power switches, toggled continuously) never found a single database bug that simulation missed. Determinism made that possible.

Everything in moonpool builds on this foundation. Chaos testing, assertion coverage, multiverse exploration, all of it requires one guarantee: given the same seed, the simulation produces the same result. The rest of this part explains how we achieve that guarantee.
