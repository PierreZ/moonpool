# Reproducing with FixedCount

<!-- toc -->

The simulation report told you that seed `17429853261` failed. The first step is always the same: reproduce it.

## Pinning a Seed

Take your simulation binary and add two things: the failing seed, and a single iteration.

```rust
SimulationBuilder::new()
    .workload(MyWorkload::new())
    .set_iterations(1)
    .set_debug_seeds(vec![17429853261])
    .run()
    .await
```

`set_debug_seeds` fixes the RNG seed. `set_iterations(1)` tells the runner to execute exactly one iteration with that seed instead of sweeping through random seeds. Together, they replay the exact execution that failed.

Run it. You should see the same failure, the same panic message, the same assertion violation. Every time.

## Turning Up the Logging

By default, simulation runs are quiet. When debugging a specific seed, turn up logging via the `RUST_LOG` environment variable. Start with `ERROR` to see assertion violations and failure messages:

```bash
RUST_LOG=error cargo xtask sim run my_simulation
```

For deeper investigation, `RUST_LOG=trace` shows the full event trace: every event processed, every timer fired, every connection state change. This is the firehose, but with a pinned seed it is a reproducible firehose.

## When the Bug Disappears

You pinned the seed, ran it, and the bug is gone. This means one thing: **something in your code is non-deterministic**.

The simulation engine controls time, networking, and randomness through providers. But if your code bypasses those providers, it introduces real-world non-determinism into the simulated world. Common culprits:

- **Direct tokio calls**: `tokio::time::sleep()` instead of `time.sleep()`, `tokio::spawn()` instead of `task_provider.spawn_task()`. These escape the simulation's control.
- **System randomness**: Using `rand::thread_rng()` or `std::collections::HashMap` iteration order instead of the simulation's `RandomProvider`.
- **Wall-clock time**: Calling `std::time::Instant::now()` or `SystemTime::now()` instead of using the simulated clock.
- **External I/O**: Reading files, making real network calls, or anything that touches the actual operating system.

If you suspect non-determinism, run the same seed twice with `RUST_LOG=trace` and diff the output. The first divergence point tells you exactly where determinism broke.

## From Reproduction to Investigation

Once you have a reliable reproduction, the next step is understanding what happened. The event trace (covered in the next section) shows you the causal chain: which events fired, in what order, and what state they produced. With a pinned seed, you can also set breakpoints in your IDE and step through the exact execution path that triggers the bug.

The key insight: you are not searching for a bug anymore. You are reading a recording of a bug. That is a fundamentally easier problem.
