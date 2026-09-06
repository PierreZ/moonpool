---
name: running-simulations
description: Configure and run a moonpool simulation end to end - SimulationBuilder wiring (processes, workloads, chaos, iteration control, chaos_duration, run_time_budget), reading the SimulationReport, and running or registering simulation binaries with cargo xtask sim. Use when writing a sim binary's main, choosing FixedCount vs UntilCoverageStable, interpreting seeds_failing / assertion_violations / coverage_violations, or adding a new example that CI must run.
---

# Running simulations

`SimulationBuilder` (`crates/moonpool-sim/src/runner/builder.rs`) is the one
entry point. It is **synchronous**: `.run()` returns a `SimulationReport`, there
is no `.await` and no tokio runtime around it. The builder creates the
deterministic executor per seed itself.

## The canonical binary

```rust
use std::process;
use std::time::Duration;
use moonpool_sim::{Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode};

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);   // verbosity lives here, not in RUST_LOG

    let report = moonpool_sim::SimulationBuilder::new()
        .processes(3, || Box::new(MyServer::new()))          // one group, IPs 10.0.1.{1..3}
        .workload_factory(|| Box::new(MyWorkload::default())) // factories replay from a fresh builder
        .enable_chaos([
            Chaos::Network(ChaosMode::Swarm),
            Chaos::Storage(ChaosMode::Swarm),
            Chaos::Attrition {
                config: Attrition {                           // no Default: name all eight fields
                    max_dead: 1,
                    prob_graceful: 0.3,
                    prob_crash: 0.5,
                    prob_wipe: 0.2,
                    recovery_delay_ms: None,
                    grace_period_ms: None,
                    scope: AttritionScope::PerProcess,
                    victims: AttritionVictims::Any,
                },
                mode: ChaosMode::Swarm,
            },
            Chaos::BuggifyKnobs,
        ])
        .chaos_duration(Duration::from_secs(10))              // required by Attrition; after it, recovery mode
        .run();

    report.eprint();
    if !report.seeds_failing.is_empty() {
        process::exit(1);
    }
}
```

`workload(instance)` exists for a one-off run, but exploration and the
determinism canary need the factory forms (`workload_factory`, `workloads`,
`processes`) because they rebuild the world from scratch per timeline.

## Choosing the iteration control

| Call | Meaning | Use when |
|---|---|---|
| default | `UntilCoverageStable { plateau_seeds: 10, max_iterations: 1000 }` | the normal sweep: stop once every sometimes/reachable fired and coverage plateaued |
| `.until_coverage_stable(p, max)` | tune the plateau | a heavy simulation that needs a bigger cap |
| `.set_iterations(n)` | `FixedCount(n)` | a smoke test, or `1` with `.set_debug_seeds(vec![seed])` to replay one failure |
| `.run_time_budget(d)` | cap per-seed simulated time (default one simulated hour) | a workload that can legitimately idle |
| `.check_determinism()` | run every seed twice and compare draw fingerprints | after any change to randomness, scheduling or lifecycle (see `/debug-a-seed`) |
| `.swarm_operations()` | per-seed subset of each workload's operation alphabet | any workload with more than a handful of operations (see `/writing-a-workload`) |

Under `cargo xtask sim run`, "coverage" means real sancov edge coverage of the
crate under test; under plain `cargo run` it falls back to assertion coverage.
That is why the sweep that has to *saturate* always goes through xtask.

## Topology knobs

- `.processes(count, factory)` — one **group** per call, named after
  `Process::name()`; group *g* lives on `10.0.g.x`, and `count` accepts a
  `usize` or a `RangeInclusive<usize>` drawn per seed. Duplicate names panic.
- `.cluster(LocalityConfig, factory)` — failure domains (datacenter / zone /
  machine) so attrition can take a whole machine.
- `.tags(&[("role", &["leader", "follower"])])?` — round-robin over the last
  group; returns `Result`, so `?` it.
- `.workloads(WorkloadCount::Fixed(n) | Random(range), |i| ...)` — several
  clients; each sees `ctx.client_id()` / `ctx.client_count()`.
- `.fault_factory(|| Box::new(MyFaults))` — a scripted `FaultInjector`
  (see `/scripting-faults`).
- `.invariant(...)` / `.invariant_fn(name, |q, t| ...)` — cross-process checks
  over captured tracing events (see `/events-and-invariants`).
- `.link_latency`, `.tcp_send_window_bytes`, `.network_fault_mask` — network
  shape; `.metrics_factory` / `.metric` — application metrics
  (`moonpool-prometheus`).

## Reading the report

`report.eprint()` prints the summary. The fields that decide pass/fail:

- `seeds_failing` — a seed is failing when its run returned `Err` **or** an
  always-assertion was violated. This is the CI gate.
- `assertion_violations` — every `assert_always*` / `assert_unreachable!`
  message that failed, plus "assertion slot table overflowed" when more than
  512 distinct messages were registered.
- `coverage_violations` — every `assert_sometimes*` / `assert_reachable!` that
  was evaluated but never satisfied. A non-empty list after a full sweep means
  the harness does not reach what it claims to; it is a finding, not noise.
- `dropped_assertion_allocations`, `saturation`, `exploration`,
  `assertion_details`, `bucket_summaries` — diagnostics.

Assertions never panic. A failing `assert_always!` records and continues, so
one root cause shows its whole cascade in a single seed; read the first
violation, not the last.

## xtask

```bash
cargo xtask sim list [filter]              # registered binaries
cargo xtask sim run <filter> [-- args]     # builds with sancov (SANCOV_CRATES) into target/sancov
cargo xtask sim run-all
```

xtask has no flags of its own; seeds and iteration counts are set in the
binary's code. To make a new example part of CI: add the `[[bin]]` under
`crates/moonpool-sim-examples/src/bin/sim/`, register it in `SIM_BINARIES` in
`crates/xtask/src/main.rs`, and add it to the `sim` matrix in
`.github/workflows/rust.yml`. All three, in the same change.

Book: `book/src/part3-building/04-simulation-builder.md`, `05-running.md`,
`appendix/03-configuration.md`; the agent-oriented walkthrough is
`book/src/llms.md`.
