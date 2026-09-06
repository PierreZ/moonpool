---
name: debug-a-seed
description: Root-cause a failing seed in moonpool's own test suite or example simulations - a red seed in cargo nextest, a seeds_failing entry from cargo xtask sim run, or a determinism-canary trip - by pinning the seed (set_debug_seeds + set_iterations(1)), raising trace_level, reading the timeline backwards from the first violation, replaying an exploration BugRecipe, and bisecting non-determinism with rng_call_count / set_rng_breakpoints. Use whenever CI or a local run reports a failing seed, an assertion violation, or "determinism canary" in this repository.
argument-hint: [seed] [test or sim binary]
---

# Debug a seed

In this repository a failing seed almost always means the *runtime* changed
behaviour: a new draw moved the schedule, an engine returned a completion in
a different order, a waker fired while the world lock was held. The seed is a
complete replay of that; find the first violation, walk back to the runtime
decision, fix the runtime. Never delete or weaken the assertion that caught
it, in a test or in an example.

## 1. Reproduce alone

- **A nextest test**: most tests build their own `SimulationBuilder`; add
  `.set_iterations(1).set_debug_seeds(vec![seed])` locally (do not commit the
  pin) and run `nix develop --command cargo nextest run -p moonpool-sim
  <test_name> --nocapture`. Low-level provider tests seed `SimWorld::new_with_seed(seed)`
  and drive with the `drive` helper from the root `AGENTS.md`.
- **An example** (`cargo xtask sim run <name>`): the binaries in
  `crates/moonpool-sim-examples/src/bin/sim/` set seeds in code, not on the
  CLI; edit the builder call the same way and rerun through xtask (sancov) or
  `cargo run --bin` (faster, coverage-blind).
- **An exploration find**: the report carries `BugRecipe { seed, recipe }`;
  use `.replay_timeline(seed, recipe)` to reproduce the exact timeline.
- Raise verbosity with `init_sim_tracing(Level::DEBUG)` and
  `.trace_level(LevelFilter::TRACE)`; there is no `RUST_LOG` plumbing.

## 2. Read the timeline backwards

Start at the first `[ASSERTION FAILED]` (later ones are cascade). The event
kinds and where they are produced:

| Event | Producer |
|---|---|
| `Timer` | `sim/sleep.rs` via the `Scheduler` |
| `DataDelivery` / `FinDelivery` / `ConnectionReady` / `PartitionRestore` | `network/sim/engine.rs` |
| `Storage` completions | `storage/sim/engine.rs` (all ops are `Pending` then an event) |
| `ProcessGracefulShutdown` / `ProcessForceKill` / `ProcessRestart` | `runner/process_manager.rs` |
| `sim_fault` (`kind = ..`) | `chaos/fault_events.rs`, merged from `SimWorld::take_faults()` |

Same-time events are FIFO by the scheduler's sequence number; if two
completions swapped order, look for a changed `schedule_at`/`schedule_after`
call or a draw inserted before it.

## 3. The seed does not reproduce

Then a draw left the single stream. Run the canary tests
(`tests/determinism_canary.rs`, `tests/determinism.rs`) or add
`.check_determinism()` to the failing builder: the violation names the first
diverging draw (`DeterminismViolation::Diverged { index, .. }`, or
`ExtraDraws` / `Unconsumed`). Bisect toward that index with
`rng_call_count()` and `set_rng_breakpoints`. In the runtime the culprits are:
a `rand::thread_rng()` or `HashMap` iteration inside an engine, a wall-clock
read in `runner/wall_clock.rs` leaking into a decision, a thread-local or
static not reset in `reset_per_iteration_state`, a detached task from a
previous iteration, or a `select!` not routed through `install_select_offset`.

## 4. Fix and widen

Fix the runtime, rerun the seed, then the subsystem's test file, then
`cargo xtask sim run-all` (every example is a regression surface for the
scheduler), then `/validate`. Cite the seed in the commit; a pinned seed
stops reproducing the moment the draw schedule moves, which in this
repository is every runtime change.

Book: `book/src/part3-building/20-debugging.md` through `23-pitfalls.md`;
`part2-foundations/03-seeds.md`.
