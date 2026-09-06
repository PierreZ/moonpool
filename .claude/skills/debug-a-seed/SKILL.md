---
name: debug-a-seed
description: Reproduce and root-cause a failing moonpool simulation seed - pin it with set_debug_seeds + set_iterations(1), raise trace_level / init_sim_tracing, read the timeline backwards from the first violation, replay an exploration BugRecipe with replay_timeline, and hunt non-determinism with check_determinism (DeterminismViolation names the first diverging draw). Use when a seed is in seeds_failing, an assertion violation appears in a report, a reproduced seed does not fail, a canary trips, or CI fails on a simulation binary.
---

# Debug a seed

A failing seed is a complete, replayable description of a bug. The job is to
replay it alone, find the **first** violation, walk the causal chain back to
the decision that made it possible, fix that decision, and then let the seed
go. Never "fix" a seed by deleting or weakening the assertion that caught it.

## 1. Reproduce it alone

```rust
moonpool_sim::init_sim_tracing(tracing::Level::DEBUG);   // process-wide subscriber floor
let report = SimulationBuilder::new()
    /* identical wiring to the failing run */
    .set_iterations(1)                                     // IterationControl::FixedCount(1)
    .set_debug_seeds(vec![17_429_853_261])
    .trace_level(tracing::level_filters::LevelFilter::TRACE) // what the timeline captures
    .run();
report.eprint();
```

`run()` is synchronous. There is no `RUST_LOG` plumbing and no `--seed`
flag on `cargo xtask sim run`: verbosity and the seed are set in code, then
run the binary again (`cargo xtask sim run <name>`, or `cargo run --bin`
when you do not need sancov).

A failure found by exploration comes with a `BugRecipe { seed, recipe }` in
`report.exploration`; replay that exact timeline with
`.replay_timeline(seed, recipe)` instead of the bare seed.

## 2. Read the trace backwards

Start at the first `[ASSERTION FAILED]` line (later ones are usually the
cascade) and walk back. The events that explain most failures:

| Event | Meaning |
|---|---|
| `Timer` | a sleeping task woke; check what it assumed had already happened |
| `DataDelivery` / `FinDelivery` | bytes or a close arrived on a stream |
| `ConnectionReady` / `PartitionRestore` | connect completed / a partition healed |
| `Storage` | a disk op completed, possibly with an injected fault |
| `ProcessGracefulShutdown` / `ProcessForceKill` / `ProcessRestart` | a reboot step; what state did the factory rebuild from |
| `sim_fault` (kind = ..) | the injected fault, in the captured timeline with `source = "sim"` |

Ask, in order: what fact did the code assume, which event should have
established it, and which fault or ordering meant it never did. The bug is
almost always in the gap between "I sent it" and "it was durably received".

## 3. The seed does not fail on replay

Then something outside the seed influenced the first run. Run the campaign
under the canary:

```rust
.check_determinism()   // every seed twice; second run must match draw for draw
```

A mismatch is reported as the always-assertion
"determinism canary: replay matched the recorded draw sequence" and a
`DeterminismViolation`: `Diverged { index, expected, actual }` names the
**first** draw whose fingerprint differs, `ExtraDraws` / `Unconsumed` say the
replay drew more or fewer times. Bisect toward that draw index with
`rng_call_count()` / `set_rng_breakpoints`. The usual culprits:

- a direct tokio call (`tokio::spawn`, `tokio::time`, `tokio::select!`);
- `HashMap`/`HashSet` iteration order feeding a decision (use `BTreeMap`);
- `Instant::now()` / `SystemTime::now()` instead of `time.now()`;
- `rand::thread_rng()` instead of `ctx.random()` / `sim_random`;
- a `static` or thread-local that survives from one run to the next;
- a detached task that outlives its run and gets polled by the next one.

Every random decision in moonpool is one counted stream (`sim/rng.rs`), so
"same seed, different run" always means a draw came from outside it.

## 4. Fix, then widen

Fix the root cause in the system under test (or in the harness when the
harness lied). Rerun the seed, then the whole campaign under
`cargo xtask sim run`, then the nextest suite (`/validate`). Cite the seed in
the commit message as the evidence it was; do not pin it as a test. Any
change to the code's draw schedule (a new buggify site, a reordered await)
makes every old seed name a different run, so a pinned witness stops testing
what it was written for the moment anything moves.

Book: `book/src/part3-building/20-debugging.md`, `21-reproducing.md`,
`22-event-trace.md`, `23-pitfalls.md`; sources: `crates/moonpool-sim/src/sim/rng.rs`,
`tests/determinism_canary.rs`.
