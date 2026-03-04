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

## Book Chapters

- `book/src/part2-foundations/10-workload.md` — Workload trait, lifecycle, SimContext
- `book/src/part3-building/03-writing-workload.md` — step-by-step walkthrough
- `book/src/part3-building/18-designing-workloads.md` — operation alphabet, invariant patterns, concurrency
