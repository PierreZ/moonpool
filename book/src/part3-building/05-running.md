# Running and Observing

<!-- toc -->

You have a Process, a Workload and a builder. Now we run the campaign, read the report, and learn what stops a run.

## Two Ways to Run

A simulation is ordinary Rust that calls `SimulationBuilder::run()`. There is no runtime to build: `run()` drives each seed on a fresh [deterministic executor](../part2-foundations/11-executor.md) on one OS thread. A bare `tokio::spawn` inside it panics, because there is no tokio runtime.

**As a test.** Put the builder in a `#[test]` and run it with nextest:

```bash
nix develop --command cargo nextest run -p moonpool-sim process_groups --nocapture
```

Fast, coverage-blind, and the way every integration test under `crates/moonpool-sim/tests/` runs.

**As a binary.** The examples in `crates/moonpool-sim-examples/src/bin/sim/` are the pattern. xtask builds them with sancov instrumentation and runs them:

```bash
cargo xtask sim list            # every simulation binary
cargo xtask sim run tonic       # binaries whose name matches
cargo xtask sim run-all         # all of them
```

Seeds and iteration counts are builder calls, not CLI flags. A binary's `main` is short:

```rust
fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = SimulationBuilder::new()
        .processes(3, || Box::new(KvServer))
        .workload(KvWorkload::new(200))
        .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
        .set_iterations(100)
        .run()
        .expect("simulation configuration is valid");

    report.eprint();
    if report.is_failure() {
        std::process::exit(1);
    }
}
```

`init_sim_tracing` installs a stdout `tracing_subscriber::fmt` at the given level. There is no `RUST_LOG` plumbing. `expect` is right here: an `Err` from `run()` is a configuration mistake, not a test result. [Running Simulation Binaries with xtask](../appendix/08-xtask.md) covers registering a new binary.

## Reading the Report

`report.eprint()` prints to stderr, coloured when stderr is a terminal and `NO_COLOR` is unset. `format!("{report}")` is the same text without colour. A healthy campaign looks like this:

```text
━━━ Simulation Report ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  100 iterations   100 passed   0 failed   ✓ 100.0%

  Wall Time    12ms avg        1.20s total
  Sim Time     45.23s avg
  Events       8,432 avg

━━━ Assertions (4) ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ✓  always       "kv: read matches the model"   12,847 pass  0 fail
  ✓  always       "kv: model never holds more..." 100 pass  0 fail
  ✓  sometimes    "kv: set failed under chaos"   412 / 6,514  (6.3%)
  ✓  sometimes    "kv: set succeeded"            6,102 / 6,514  (93.7%)
```

The lines that matter:

- **✓ 100.0%**: no seed returned an error, panicked or deadlocked.
- **0 fail** on an `always` row: no invariant violation.
- **✓** on a `sometimes` row: the path fired at least once. A **○** marks one that never did.

Four more sections appear when there is something to show. **Violations** lists every failed always-assertion. **Coverage Gaps** lists every sometimes or reachable assertion that never fired. **Saturated** or **Saturation FAILED** explains how an `UntilCoverageStable` run stopped. **Seeds** lists each seed with its wall time, sim time and event count, failing seeds in red.

## What Success Means

A campaign succeeds when two conditions hold at once:

1. **No always-assertion violated.** Every `assert_always!` passed on every evaluation, on every seed.
2. **Every sometimes-assertion fired.** Every `assert_sometimes!` and `assert_reachable!` was true at least once across the campaign.

Both matter. A run that never violates an invariant but never reaches an error path is not testing enough. A run that reaches every path but tolerates wrong answers is not checking enough.

`report.is_failure()` covers the first condition and more: it is true when `seeds_failing` is non-empty, when `assertion_violations` is non-empty, or when `dropped_assertion_allocations` is above zero, which means the assertion table overflowed and some evaluations were never counted. Coverage is deliberately not part of it. Check `coverage_violations` and `convergence_timeout` yourself when CI should fail on a coverage gap.

## When Things Fail

```text
━━━ Simulation Report ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  100 iterations   98 passed   2 failed   ✗ 98.0%

  Wall Time    12ms avg        1.20s total
  Sim Time     45.23s avg
  Events       8,432 avg

  Faulty seeds: [7891, 42033]

━━━ Assertions (4) ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ✗  always       "kv: read matches the model"   12,800 pass  47 fail
  ...

━━━ Violations ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  ✗  assert_always!('kv: read matches the model') failed 47 times out of 12,847
```

The report says which seeds failed, which assertion broke, and how rare it was. An assertion written with a detail block also printed `[ASSERTION FAILED] kv: read matches the model (seed=7891) | key=k3 server=10.0.1.2` on stderr the moment it failed, which is why the workload chapter attached the key and the server. [Debugging a Failing Seed](./20-debugging.md) takes it from here: pin seed 7891 with `set_debug_seeds(vec![7891])` and `set_iterations(1)`, raise the trace level, read backwards from the first violation.

## Stop Conditions

How long should a campaign run? Any fixed count is wrong: too small misses bugs, too large burns time. The right answer is to **stop on a signal, not a count**.

**`UntilCoverageStable { plateau_seeds, max_iterations }`** is the default. It stops when every observed sometimes, numeric-sometimes and reachable assertion has fired **and** coverage has not grown for `plateau_seeds` consecutive seeds. Under `cargo xtask sim run` the binary is sancov-instrumented, so the signal is **real code coverage**, the count of distinct edges exercised. Under `cargo nextest run` there is no instrumentation and it falls back to **assertion coverage**, the set of coverage messages that fired. The **Saturated** section names which signal it used. A campaign with no coverage assertion stops on the plateau alone and logs a warning. `max_iterations` is a safety cap: hitting it sets `report.convergence_timeout = true` and prints **Saturation FAILED**, so CI can fail loudly instead of treating "we ran out of seeds" as success. No fork happens unless `enable_exploration` is set.

**`FixedCount(n)`** runs exactly `n` seeds. Use it to replay, and when a budget must be predictable.

## Stall Detection

A seed must end. Four deterministic guards make sure it does, each a pure function of the simulated schedule, so a replay hits the same guard at the same step.

- **No progress.** More than three consecutive loop passes with an empty event queue and no workload finished trigger a shutdown request. The same count again after the request declares a deadlock.
- **Same-time loop.** 100,000 consecutive events at one logical time, such as a re-armed zero-duration sleep, trigger a shutdown request, then a deadlock.
- **Run-time budget.** Simulated time past `run_time_budget` (one simulated hour by default) with workloads still running triggers a shutdown request. Another full budget past that declares a deadlock. This catches a detached task that re-arms a timer every tick while the workload waits on it, which the first guard cannot see. Setup and check phases use the same guard.
- **Settle timeout.** After every workload returns, processes are aborted and the orchestrator drains the queue without yielding. Five simulated minutes of events still pending fails the seed with `SimulationError::SettleTimeout`, which surfaces leaked timers and unclosed connections.

A deadlocked seed stops the whole campaign. The report still carries everything earlier seeds gathered, with the deadlocked seed in `seeds_failing` and `convergence_timeout` left false.

Two more things end a campaign early. A workload task that panics or is cancelled before it returns its `Box<dyn Workload>` stops the run, because reusing the survivors would hand one workload another's identity. An ordinary `Err` from a workload keeps the instance and fails only that seed. A process that panics fails its seed as well.

## Checking Determinism

`check_determinism()` runs every seed twice and compares a fingerprint after every draw on the simulation stream. The replay is a full run, judged like one. Any uncontrolled difference, a `HashMap` iterated in random order, a wall-clock read, a static that survives a run, fails the seed with the always-assertion `"determinism canary: replay matched the recorded draw sequence"` and names the first diverging draw. It doubles the cost and needs factory workloads. Turn it on when a seed stops reproducing, and [Reproducing with FixedCount](./21-reproducing.md#when-the-bug-disappears) shows how to bisect from there.

## Exit Codes

A binary exits 0 on success and 1 on failure. Gate on `is_failure()`:

```rust
if report.is_failure() {
    eprintln!(
        "ERROR: {} seeds failed {:?}, {} violations, {} dropped allocations",
        report.seeds_failing.len(),
        report.seeds_failing,
        report.assertion_violations.len(),
        report.dropped_assertion_allocations
    );
    std::process::exit(1);
}
```

Gating on `seeds_failing` alone lets a run with partial assertion accounting pass. The examples' `finish_or_exit_on_failure` helper in `crates/moonpool-sim-examples/src/support.rs` is exactly this.

## The Feedback Loop

Write a workload, run it, find a bug, fix the bug, add an assertion so it stays fixed, run again. The assertion table is the scoreboard. Every row ✓ with high counts means the test is thorough and the system is correct. A ○ means a path the chaos is not reaching. A ✗ means a real bug.
