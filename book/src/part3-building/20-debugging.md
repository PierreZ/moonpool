# Debugging a Failing Seed

<!-- toc -->

A seed failed. The report printed `Faulty seeds: [17429853261]` and a red row in the assertion table. What now?

In production, debugging a concurrency bug means staring at interleaved logs from several nodes and trying to reconstruct a sequence you cannot replay. You form a theory, add logging, deploy, wait for the bug to come back, and hope the new lines captured enough. It is slow and often inconclusive.

Deterministic simulation changes this. A failing seed is not a clue. It is a **recording**. Same seed, same schedule, same draws, same bug, every time. Debugging becomes mechanical rather than archaeological.

## The Workflow

1. **Reproduce alone.** Pin the seed with `set_debug_seeds(vec![seed])` and `set_iterations(1)`. Raise the trace level. Confirm the same violation appears.
2. **Read the timeline backwards.** Start at the **first** `[ASSERTION FAILED]` line. Later ones are usually cascade. Walk back through the events before it to the decision that caused it.
3. **Understand the causal chain.** Which delivery landed a stale message? Which timer fired? Which process was mid-reboot? Name the runtime decision, not just the symptom.
4. **Fix the root cause** in the system under test. Never delete or weaken the assertion that caught it.
5. **Widen.** Rerun the seed. Rerun the test file. Run `cargo xtask sim run-all`, because every example is a regression surface for the scheduler. Cite the seed in the commit.

Each step is simple because determinism gives us repeatability. We do not hunt ghosts. We replay recordings.

## What Makes This Different

Traditional debugging assumes non-determinism. You set a breakpoint and hope the thread schedule cooperates. You add a print and hope the race still happens. You run the test ten times and it passes nine.

With simulation, the seed controls everything: which connections fail, when timers fire, which ready task runs next, how long a delay takes, which fault families are on. Pin the seed and the execution is frozen. Add logging, set breakpoints, restructure the investigation, and the bug is still there, exactly where you left it.

One consequence is easy to miss. A pinned seed stops reproducing the moment the draw schedule moves, and in this repository the draw schedule moves with **every runtime change**: a new random decision, a reordered await in the runner, a new chaos surface on the builder. That is intended. The seed is a replay of one schedule, not a stable test case. When the fix changes the schedule, confirm it by running the campaign again, not by replaying the old seed.

## The Three Chapters That Follow

[Reproducing with FixedCount](./21-reproducing.md) covers pinning a seed in a test, in an example binary, and from an exploration recipe, how to turn up the logging, and what to do when the bug refuses to come back.

[Reading the Event Trace](./22-event-trace.md) covers the event kinds the scheduler processes, where each one is produced, and how to read the timeline backwards from a violation.

[Common Pitfalls](./23-pitfalls.md) is the list of mistakes we have made ourselves, so you can recognise them in one glance.

## A Note on Non-Determinism

If the pinned seed runs clean, you have a different problem: something in the system is **non-deterministic**. That is valuable information. The usual sources are a direct `tokio` call that bypasses the providers, a `HashMap` whose iteration order leaks into behaviour, a wall-clock read, or a static that survives from one seed into the next. `check_determinism()` on the builder runs the seed twice and names the first draw where the two runs disagree. The next chapter shows how to bisect from there.
