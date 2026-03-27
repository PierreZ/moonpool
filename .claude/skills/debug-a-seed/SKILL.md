---
description: |
  Debug failing moonpool simulation seeds: reproduce, trace events, find root cause, fix.
  TRIGGER when: a simulation seed fails, investigating a test failure, reading event traces, or hunting non-determinism bugs.
  DO NOT TRIGGER when: not debugging moonpool simulation failures.
---

# Debug a Seed

## When to Use This Skill

Invoke when:
- A simulation seed failed and you need to investigate
- Reproducing a specific failure deterministically
- Reading event traces to find root cause
- A reproduced seed doesn't fail (non-determinism hunt)

## Quick Reference

**Step 1: Reproduce**
```rust
SimulationBuilder::new()
    .workload(MyWorkload::new())
    .set_iterations(1)
    .set_debug_seeds(vec![17429853261])
    .run()
    .await
```

**Step 2: Turn up logging**
```bash
RUST_LOG=error cargo xtask sim run my_simulation   # just failures
RUST_LOG=trace cargo xtask sim run my_simulation    # full event trace
```

**Step 3: Read the trace backwards** from the assertion failure. Key event types:
- `Timer` — wakes sleeping tasks
- `DataDelivery` / `FinDelivery` — network data and close signals
- `ConnectionReady` / `PartitionRestore` — connection state changes
- `Storage` — disk I/O with possible faults
- `ProcessGracefulShutdown` / `ProcessForceKill` / `ProcessRestart` — reboots

**Step 4: Fix and verify** — re-run the failing seed, then the full chaos suite.

**Bug disappears on reproduction?** Non-determinism sources to check:
- Direct tokio calls bypassing providers
- `HashMap` iteration order leaking into behavior
- `std::time::Instant::now()` or `SystemTime::now()` instead of simulated clock
- `rand::thread_rng()` instead of `RandomProvider`

**Tip**: Run the same seed twice with `RUST_LOG=trace` and diff the output to find the divergence point.

## Book Chapters

- `book/src/part3-building/19-debugging.md` — the 5-step debugging workflow
- `book/src/part3-building/20-reproducing.md` — pinning seeds with FixedCount, handling non-determinism
- `book/src/part3-building/21-event-trace.md` — event types, causal chain tracing, RNG call counts
- `book/src/part3-building/22-pitfalls.md` — common mistakes reference (storage step loop, unwrap, LocalSet, etc.)
