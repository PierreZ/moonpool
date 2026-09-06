---
name: changing-the-runner
description: Change moonpool's runner layer - SimulationBuilder methods and defaults, the per-iteration lifecycle (reset_per_iteration_state - RNG reseed, select offset, violation reset, buggify_init - then swarm mask, canary record/check), process groups and IP allocation in resolve_process_config, iteration control and stall/run_time_budget, the SimulationReport, and registering a new example with xtask and the CI matrix. Use when adding or changing a builder method, a Chaos or ChaosMode variant, a report field, the orchestrator loop, process/workload lifecycle, or an example simulation.
---

# Changing the runner

`crates/moonpool-sim/src/runner/` is where a seed becomes a run: the builder
collects configuration, `orchestrator.rs` / `iteration.rs` drive one
iteration per seed, `process_manager.rs` boots and reboots processes,
`report.rs` aggregates. Every consumer's `main` is written against the
builder, so an addition here is public API with a book chapter, and the
per-iteration lifecycle is where determinism is won or lost.

## The per-iteration lifecycle (`builder.rs`)

1. `reset_per_iteration_state(seed, ..)`: reset the observability layer for
   the seed, `reset_sim_rng` + `set_sim_seed(seed)`, `install_select_offset`
   (route `select!` branch offsets through the stream), `reset_always_violations`,
   `buggify_init(0.5)`.
2. Draw the per-seed shape **in registration order**: each process group's
   count, localities and tags (`resolve_process_config`; group *g* lands on
   `10.0.{g+1}.{1..=N}`, workloads on `10.0.0.x`), the chaos surfaces'
   `random_for_seed` / `swarm_for_seed`, the operation swarm mask
   (`draw_swarm_op_mask`, exactly four draws) when `swarm_operations()` is on.
3. Under `check_determinism()`: `begin_determinism_record` on the first run
   of a seed, `begin_determinism_check` on the second, `finish_determinism_check`
   after; a mismatch becomes the always-assertion
   "determinism canary: replay matched the recorded draw sequence".
4. Run: `setup` sequentially, `run` concurrently with processes and fault
   injectors, chaos until `chaos_duration` then recovery mode, `check` after
   quiescence; `stall.rs` caps simulated time at `run_time_budget` (one
   simulated hour by default) and detects a world with only infrastructure
   events left.
5. `validate_assertion_contracts`, merge `take_faults()` into the timeline,
   record the seed as failing on `Err` or an always violation.

Anything you add that draws randomness goes in step 2 (before processes
start) or through a provider, in a fixed number of draws, so seeds stay
replayable and the recipe format of the explorer stays valid.

## Adding a builder method

- `#[must_use] pub fn name(mut self, ..) -> Self`, documented with the default
  and its effect on determinism (does it draw? when?). `tags` is the one
  `Result`-returning exception, because a bad dimension is a caller error.
- Defaults live in `new()` (`UntilCoverageStable { plateau_seeds: 10,
  max_iterations: 1000 }`, `trace_level = INFO`, `NetworkFaultMask::all()`,
  baseline network faults **on** without `enable_chaos`).
- New enum shapes go in `runner/config.rs` (`Chaos`, `ChaosMode`,
  `IterationControl`, `WorkloadCount`, `ProcessCount`) and `runner/process.rs`
  (`Attrition`, `RebootKind`, `AttritionScope`, `AttritionVictims`).
- Expose to processes and workloads through `SimContext` (`runner/context.rs`)
  and to fault injectors through `FaultContext` (`runner/fault_injector.rs`);
  keep the two in step (`topology()` vs `ips_in_group`, `state()` in both).
- Report: a new fact is a field on `SimulationReport` printed by
  `runner/display.rs`, never a log line someone has to grep.
- Tests: `tests/sim.rs`, `process_groups.rs`, `topology.rs`, `reboot.rs`,
  `swarm_op_alphabet.rs`, `run_time_budget.rs`, `metric_queries.rs` pin the
  builder features by name; add one.
- Book: `part3-building/04-simulation-builder.md`, `05-running.md`,
  `appendix/03-configuration.md`, and `llms.md` (`/update-the-book`).

## Adding or changing an example

Three registrations or CI never runs it: the module + `[[bin]]` in
`crates/moonpool-sim-examples/src/bin/sim/`, `SIM_BINARIES` in
`crates/xtask/src/main.rs` (crate name with underscores for sancov), and the
`sim` matrix in `.github/workflows/rust.yml`. An example's `main` is the
reference shape consumers copy: `init_sim_tracing`, factories, chaos,
`report.eprint()`, exit 1 when `seeds_failing` is non-empty.
