# Running and Observing

<!-- toc -->

You have a Process, a Workload, and a builder configuration. Now we run the simulation and make sense of the output.

## Running with xtask

The primary way to run simulation binaries is through xtask:

```bash
cargo xtask sim list         # List all simulation binaries
cargo xtask sim run kv       # Run binaries matching "kv"
cargo xtask sim run-all      # Run everything
```

The `run` subcommand matches against binary names. `cargo xtask sim run transport` would run both `sim-transport-e2e` and `sim-transport-messaging`.

Each simulation binary is a standalone Rust binary that constructs a `SimulationBuilder`, calls `.run().await`, and prints the report. A typical `main` function:

```rust
fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("Failed to build runtime");

    let report = runtime.block_on(async move {
        SimulationBuilder::new()
            .processes(3, || Box::new(KvServer))
            .workload(KvWorkload::new(200, keys))
            .set_iterations(100)
            .random_network()
            .run()
            .await
    });

    report.eprint();

    if !report.seeds_failing.is_empty() || !report.assertion_violations.is_empty() {
        std::process::exit(1);
    }
}
```

Notice `new_current_thread().build()`. Determinism still demands a single OS thread, so we keep the current-thread scheduler. What changed is that every moonpool trait and future is now `Send + 'static`, which means the standard `.build()` is the correct API. Customer code can hold `Arc<RwLock<…>>`, `DashMap`, and call `tokio::spawn` naturally, while the runtime still polls everything on one thread for reproducibility.

## Reading the SimulationReport

The report prints to stderr with `.eprint()`. Here is what a healthy report looks like:

```
=== Simulation Report ===
  Iterations: 100  |  Passed: 100  |  Failed: 0  |  Rate: 100.0%

  Avg Wall Time:     12ms           Total: 1.20s
  Avg Sim Time:      45.23s
  Avg Events:        8,432

--- Assertions (4) ---
  PASS  [always    ]  "read matches model"              12,847 pass  0 fail
  PASS  [always    ]  "conservation law"                 8,200 pass  0 fail
  PASS  [sometimes ]  "set_succeeded"                    6,102 / 12,847 (47.5%)
  PASS  [sometimes ]  "set_failed_network"               412 / 12,847 (3.2%)
```

The critical lines:

- **Rate: 100.0%** means no iteration panicked or returned an error
- **0 fail** on always-assertions means no invariant violations
- **PASS** on sometimes-assertions means every coverage goal was hit at least once

## What Success Means

A simulation succeeds when **two conditions** hold simultaneously:

1. **No always-assertion violations**: Every `assert_always!` passed on every evaluation across all iterations
2. **All sometimes-assertions fired**: Every `assert_sometimes!` evaluated to true at least once across all iterations

Both matter. A simulation that never violates invariants but also never exercises error paths is not testing enough. A simulation that hits every code path but tolerates wrong answers is not checking enough.

## When Things Fail

A failing report shows faulty seeds and violations:

```
=== Simulation Report ===
  Iterations: 100  |  Passed: 98  |  Failed: 2  |  Rate: 98.0%

  Faulty seeds: [7891, 42033]

--- Assertions (4) ---
  FAIL  [always    ]  "read matches model"              12,800 pass  47 fail
  PASS  [always    ]  "conservation law"                 8,200 pass  0 fail
  PASS  [sometimes ]  "set_succeeded"                    6,102 / 12,847 (47.5%)
  PASS  [sometimes ]  "set_failed_network"               412 / 12,847 (3.2%)

--- Assertion Violations ---
  - Always "read matches model": 47 failures out of 12,847 evaluations
```

The report tells you:

- **Which seeds failed**: 7891 and 42033
- **Which assertion broke**: "read matches model" had 47 failures
- **How often**: 47 out of 12,847 evaluations, so it is a rare condition

## Debugging a Failing Seed

Take the failing seed and isolate it:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
    .workload(KvWorkload::new(200, keys))
    .set_debug_seeds(vec![7891])
    .run()
    .await;
```

Now increase logging. Set the environment variable:

```bash
RUST_LOG=debug cargo xtask sim run kv
```

Because the simulation is deterministic, seed 7891 reproduces the exact same scheduling, the exact same random choices, the exact same failure. You can add `tracing::debug!` statements, rerun with the same seed, and see exactly what happened.

The debugging workflow:

1. **Find the seed** in the report's faulty seeds list
2. **Isolate it** with `set_debug_seeds(vec![seed])`
3. **Add logging** in the Process and Workload code
4. **Rerun** and trace the execution
5. **Fix** the root cause in the Process code
6. **Verify** by running the full iteration suite again

## Stop Conditions

How long should a chaos run last? Moonpool exposes four answers via `IterationControl`, each suited to a different question.

**`FixedCount(n)`** is the workhorse for CI. You commit to running exactly `n` seeds, the duration is bounded, and the report is reproducible across machines. Reach for this when budgets are predictable.

**`TimeLimit(d)`** is the answer when "as much chaos as we have time for" is the right framing. Long-running soak runs and overnight loops use this. The seed count is unbounded but the wall clock is.

**`UntilConverged { max_iterations }`** stops when every `assert_sometimes!` has fired at least once **and** the explorer's coverage bitmap stopped growing on the latest seed. It requires `.enable_exploration()` because it reads the fork-aware coverage map. Use it when you have configured exploration and want the run to end as soon as it has nothing left to discover.

**`CoveragePlateau { plateau_seeds, require_all_sometimes, max_iterations }`** is the more general plateau detector. It tracks the set of `assert_sometimes!` / `assert_reachable!` messages that have ever fired and stops once that set has not grown for `plateau_seeds` consecutive seeds. The signal lives in moonpool-sim, so this works **with or without** fork-based exploration. Set `require_all_sometimes=true` if you want to refuse to stop until every observed sometimes assertion has fired at least once.

```rust
SimulationBuilder::new()
    .workload(KvWorkload::new(200, keys))
    .until_coverage_plateau(50, 5_000)
    .run();
```

Both `UntilConverged` and `CoveragePlateau` set `report.convergence_timeout = true` when the safety cap is hit before the run converges, so CI can fail loudly instead of silently treating "we ran out of seeds" as success.

## cargo nextest vs cargo xtask sim

Moonpool has two ways to run tests:

**`cargo nextest run`** runs unit tests and integration tests. These are fast, focused tests for specific modules. Use nextest during development for quick feedback.

**`cargo xtask sim run`** runs simulation binaries. These are comprehensive, multi-iteration chaos tests that take longer but find deeper bugs. Use xtask for validation before merging.

Both should pass before work is complete. The typical workflow: write code, run `nextest` for fast iteration, then run `xtask sim` for thorough validation.

## Exit Codes

Simulation binaries should exit with code 0 on success and code 1 on failure. The standard pattern:

```rust
if !report.seeds_failing.is_empty()
    || !report.assertion_violations.is_empty()
{
    std::process::exit(1);
}
```

Check both `seeds_failing` (iterations that panicked or errored) and `assertion_violations` (always-type assertions that failed). Coverage violations (`coverage_violations`) indicate sometimes-assertions that never fired, which may or may not be worth failing the build over depending on your testing philosophy.

## The Feedback Loop

Simulation testing is iterative. You write a workload, run it, find a bug, fix the bug, add assertions to prevent regression, run again. Each round makes the system more robust.

The report's assertion table is your scoreboard. When you see all PASS with high hit counts, you know your test is both thorough and correct. When you see MISS on a sometimes-assertion, you know there is a code path your chaos is not reaching. When you see FAIL on an always-assertion, you know there is a real bug to fix.

This is the rhythm of simulation-driven development: build, test, observe, improve.
