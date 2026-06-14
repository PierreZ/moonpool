---
description: |
  Implementing a moonpool Workload (test driver): trait, setup/run/check lifecycle, operation alphabet, reference models.
  TRIGGER when: implementing the Workload trait, writing simulation test drivers, designing operation alphabets, or setting up reference models.
  DO NOT TRIGGER when: not working on moonpool simulation code.
---

# Writing a Workload

## When to Use This Skill

Invoke when:
- Writing a test driver for a simulation
- Designing an operation alphabet
- Implementing validation logic and reference models
- Using the setup/run/check lifecycle

## Quick Reference

```rust
#[async_trait(?Send)]
pub trait Workload: 'static {
    fn name(&self) -> &str;
    async fn setup(&mut self, _ctx: &SimContext) -> SimulationResult<()> { Ok(()) }
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> { Ok(()) }
}
```

- **Lifecycle**: `setup()` (sequential) → `run()` (concurrent) → `check()` (sequential, after all events drain)
- **Survives reboots**: Workloads keep running when processes crash and restart
- **Operation alphabet**: Normal ops (~60%), adversarial inputs (~20%), nemesis ops (~20%)
- **Reference model**: Track expected state locally, compare on every response
- **Multiple instances**: `SimulationBuilder::new().workloads(WorkloadCount::Fixed(5), |i| Box::new(...))`
- **State publishing**: `ctx.state().publish("model", self.model.clone())` after every mutation for invariants
- **Operation-alphabet swarm**: per-seed subset of the alphabet via `swarm_op_enabled(op_id)` (opt in with `SimulationBuilder::swarm()`)

## Operation-Alphabet Swarm Testing

Picking operations *uniformly over the full alphabet every step* suffers **passive
suppression**: a bounded queue with 50/50 push/pop is a 1-D random walk that (by
the Hoeffding bound) needs tens of millions of ops to ever fill, so the
capacity-overflow bug is never hit. Swarm testing fixes this by enabling a random
**subset** of the alphabet per seed (the rest fully off), so extreme states
("only pushes") show up immediately.

`moonpool_sim::swarm_op_enabled(op_id: u8) -> bool` reports whether operation
`op_id` is in this seed's subset. It is:
- **Opt-in**: returns `true` for every op unless `SimulationBuilder::swarm()` is
  set — so default runs use the full alphabet (zero behavior change).
- **Pure & deterministic**: each op is independently ~50% on, a pure function of
  `(seed, op_id)`. Query it any number of times in any order — same answer. It
  consumes **no** RNG (neither `SIM_RNG` nor the fault-family `CONFIG_RNG`), so
  fork-explorer replay and fault-family swarm are unperturbed.

Pattern — remap a single full-alphabet draw into the enabled subset, preserving
the per-step `SIM_RNG` call count (critical for deterministic replay — **do not**
add a resample loop that draws extra RNG):

```rust
// Cache the enabled subset once per run (idempotent). Empty-mask fallback:
// if a seed disables everything, fall back to the full alphabet so the
// workload always has something to do.
let enabled: Vec<u8> = (0..NUM_OPS).filter(|&i| swarm_op_enabled(i)).collect();
let enabled = if enabled.is_empty() { (0..NUM_OPS).collect() } else { enabled };

// Per step: one RNG draw, remapped into the subset (== raw % NUM_OPS when full).
let raw = moonpool_sim::sim_random::<u64>();
let op = enabled[(raw % enabled.len() as u64) as usize];
```

Demonstrator: emit a coverage assertion *only on the path where it holds* (so it
never accrues a 0%-success coverage violation when unreachable):

```rust
if !enabled.iter().any(|&i| i < NUM_MOVE_OPS) {
    moonpool_sim::assert_sometimes!(true, "movement suppressed by swarm");
}
```

Reference implementation: `moonpool-sim-examples/src/dungeon.rs`
(`swarm_enabled_actions`, `pick_action_index`). End-to-end test:
`moonpool-sim/tests/swarm_op_alphabet.rs`.

## Book Chapters

- `book/src/part2-foundations/10-workload.md` — Workload trait, lifecycle, SimContext
- `book/src/part3-building/03-writing-workload.md` — step-by-step walkthrough
- `book/src/part3-building/18-designing-workloads.md` — operation alphabet, invariant patterns, concurrency
