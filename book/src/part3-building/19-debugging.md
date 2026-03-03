# Debugging a Failing Seed

<!-- toc -->

A seed failed. The simulation report printed a number, maybe `seed=17429853261`, next to a red line. What now?

In production distributed systems, debugging a concurrency bug means staring at interleaved logs from multiple nodes, trying to reconstruct a sequence of events that you cannot replay. You form a theory, add logging, deploy, wait for the bug to happen again, and hope your new logs captured enough. It is slow, painful, and often inconclusive.

Deterministic simulation changes this completely. A failing seed is not a clue. It is a **recording**. Same seed, same execution, same bug, every time. Debugging becomes mechanical rather than archaeological.

## The Workflow

The process has five steps:

1. **Reproduce** the failure with the exact seed and `FixedCount(1)`
2. **Isolate** by reading the event trace to find the triggering event
3. **Understand** the causal chain that led to the violation
4. **Fix** the root cause in your code
5. **Verify** by re-running the original seed and then the full chaos suite

Each of these steps is straightforward because determinism gives us something rare in distributed systems: repeatability. We do not hunt ghosts. We replay recordings.

## What Makes This Different

Traditional debugging tools assume non-determinism. You set a breakpoint and hope the thread schedule cooperates. You add print statements and hope the race condition still manifests. You run the test ten times and it passes nine.

With simulation, the RNG seed controls everything: which connections fail, when timers fire, what order events process, how long delays take. Pin the seed, and the entire execution is frozen in time. You can add logging, set breakpoints, restructure your investigation, and the bug will be there waiting, exactly where you left it.

The next three sections cover the practical details: how to pin a seed and reproduce, how to read the event trace, and what common mistakes look like so you can recognize them quickly.

## A Note on Non-Determinism

If you reproduce with a failing seed and the bug disappears, you have a different problem: something in your system is non-deterministic. This is actually valuable information. Common sources include direct `tokio` calls that bypass providers, `HashMap` iteration order leaking into behavior, or system randomness sneaking in through a dependency. The reproducing chapter covers how to track these down.
