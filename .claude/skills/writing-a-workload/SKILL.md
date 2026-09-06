---
name: writing-a-workload
description: Implement a moonpool Workload (the test driver) - the setup/run/check lifecycle, designing an operation alphabet with stable ids, per-seed operation swarm via swarm_operations() and swarm_op_enabled, reference models, publishing state for invariants, and multi-client workloads. Use when writing a simulation test driver, adding an operation to an existing workload, or when a check() needs to judge client-visible correctness such as linearizability.
---

# Writing a Workload

A `Workload` is the client. It outlives every process reboot inside a
timeline, so it is the only party that knows its own program order, which is
why client-visible correctness (a reference model, linearizability, "every
acked write is readable") lives in its `check()` and nowhere else.

## The trait and lifecycle

```rust
use async_trait::async_trait;
use moonpool_sim::{SimContext, SimulationResult, Workload};

#[async_trait]                                        // no (?Send)
impl Workload for MyWorkload {
    fn name(&self) -> &str { "my-workload" }
    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> { Ok(()) }  // sequential
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> { /* drive ops */ Ok(()) }  // concurrent
    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> { /* judge history */ Ok(()) } // after quiescence
}
```

`setup` runs one workload at a time, `run` runs all of them concurrently with
the processes, `check` runs after the run phase and after the world drained.
Register with `.workload_factory(|| Box::new(..))` or
`.workloads(WorkloadCount::Fixed(n) | Random(a..b), |i| Box::new(..))`;
exploration and the determinism canary need a factory because they rebuild
from a fresh builder. `ctx.client_id()` / `ctx.client_count()` identify one
client among many.

## The operation alphabet

Give every operation a stable small-integer id, keep the ids in one `const`
table, and never renumber (a swarm mask is a pure function of `(seed, id)`,
so renumbering silently changes what every seed exercises). Aim for roughly
60% ordinary operations, 20% adversarial inputs (duplicates, out-of-range
keys, retries after an ambiguous timeout), 20% nemesis-shaped operations the
client can legitimately issue (compaction requests, config changes).

Enable `.swarm_operations()` on the builder. A uniform draw over the full
alphabet is a random walk that almost never reaches an extreme state (a
bounded queue with 50/50 push/pop never fills); a per-seed subset does. Query
the subset with `moonpool_sim::swarm_op_enabled(op_id)`, which draws nothing
itself (the mask is four draws at iteration start):

```rust
// Cache once per run. If a seed disabled everything, use the full alphabet so
// the client always has something to do.
let enabled: Vec<u8> = (0..NUM_OPS).filter(|&i| swarm_op_enabled(i)).collect();
let enabled = if enabled.is_empty() { (0..NUM_OPS).collect() } else { enabled };

// One draw per step, remapped into the subset. Do NOT loop-resample: extra
// draws shift every later decision and break replay.
let raw = moonpool_sim::sim_random::<u64>();
let op = enabled[(raw % enabled.len() as u64) as usize];
```

Reference: `crates/moonpool-sim-examples/src/dungeon.rs` (`swarm_enabled_actions`,
`pick_action_index`) and `crates/moonpool-sim/tests/swarm_op_alphabet.rs`.

## Judging correctness

- **Reference model.** Track the expected state locally, compare on every
  response with `assert_always!(actual == expected, "...", { "key" => k })`.
- **Ambiguity is not failure.** A timed-out request may have been applied.
  Record it as ambiguous and let a later read settle it; retries must carry
  the same identity so the server can deduplicate.
- **Publish state** with `ctx.state().publish("model", snapshot)` after each
  mutation so a `FaultInjector` or another workload can read it, and emit
  `tracing::info!(.., "constant_event_name")` facts an `Invariant` can query
  (`/events-and-invariants`).
- **Coverage.** Mark the outcomes the run must be proven to reach with
  `assert_sometimes!`; mark a cause that merely fired (an operation the seed
  drew) with a branch-guarded `assert_reachable!`, so a seed that never draws
  it cannot fail coverage.

Tunables that shape a run (request counts, timing windows, batch sizes) are
config the workload perturbs per seed with `buggify_knob!(default, lo..hi)`,
one call site per knob; a constant buried in the process is invisible to the
swarm.

Book: `book/src/part2-foundations/10-workload.md`,
`part3-building/03-writing-workload.md`, `part3-building/19-designing-workloads.md`.
