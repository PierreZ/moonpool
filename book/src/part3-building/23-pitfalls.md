# Common Pitfalls

<!-- toc -->

A list of mistakes we have made ourselves when building simulations with Moonpool. Each one has a symptom, a cause and a fix, so you can recognise it in one glance.

## Pinning a Seed Without a Count

**Symptom**: you called `set_debug_seeds(vec![seed])`, and the run goes on for hundreds of seeds.

**Cause**: `set_debug_seeds` only supplies the first seeds. The default `UntilCoverageStable` control keeps drawing fresh ones until the plateau.

**Fix**: add `set_iterations(1)`, or `set_iterations(seeds.len())` for a list.

## Forgetting the `?` on `.tags()`

**Symptom**: a type error after `.tags(..)`: `Result<SimulationBuilder, _>` has no method `workload`.

**Cause**: `.tags()` returns `Result<Self, SimulationError>` because it fails with `InvalidState` when no `.processes()` or `.cluster()` call precedes it.

**Fix**: `.tags(&[..])?` in a function that returns `Result`, or `.expect("tags follow a group")` in a test.

## Attrition Without a Chaos Window

**Symptom**: `run()` returns `Err(InvalidConfiguration("1 Chaos::Attrition regime(s) registered without SimulationBuilder::chaos_duration ..."))`.

**Cause**: fault injectors, the built-in attrition and outages included, run only inside the chaos window. A builder without `chaos_duration` would never run them, so it is refused instead of silently passing.

**Fix**: add `.chaos_duration(Duration::from_secs(..))`. The same check applies to `fault_factory`.

## An Instance Workload Under a Rerun

**Symptom**: `run()` returns `Err(InvalidConfiguration("exploration runs a seed more than once and requires fresh workloads ..."))`, or the determinism canary trips on every seed.

**Cause**: `.workload(instance)` reuses one value across runs. Exploration and `check_determinism()` run a seed more than once and need fresh state each time.

**Fix**: `.workload_factory(|| Box::new(..))` or `.workloads(count, |i| Box::new(..))`.

## A Workload That Never Returns

**Symptom**: the log says `DEADLOCK detected on iteration .. with seed ..` after a long simulated run, and the campaign stops.

**Cause**: `run()` loops forever, or waits on a detached task that re-arms a timer every tick. The stall guards request shutdown after `run_time_budget` (one simulated hour) and declare a deadlock a budget later. A zero-duration sleep loop trips the same-time guard after 100,000 events.

**Fix**: do N operations or sleep for a fixed simulated duration, then return `Ok(())`. Honour `ctx.shutdown()` in long loops. Raise `run_time_budget` only for a legitimately long simulation.

## Direct Tokio Calls

**Symptom**: a `tokio::spawn` panics because no runtime is running, a `tokio::time::sleep` never wakes, or a seed stops reproducing.

**Cause**: there is no tokio runtime inside a simulation. `tokio::time::sleep`, `tokio::time::timeout`, `tokio::spawn` and `tokio::select!` bypass the simulated clock, the executor and the single random stream.

**Fix**: `ctx.time().sleep()`, `ctx.time().timeout()`, `ctx.task().spawn_task()` and `moonpool_sim::select!`. In production the same calls run on `TokioProviders`.

## Wrong Runtime Flavor

**Symptom**: a `#[tokio::main]` or `#[tokio::test]` around the builder, and results differ between runs.

**Cause**: Moonpool runs on **one OS thread** for determinism. Wrapping it in a tokio runtime hands scheduling to tokio: redundant at best, real parallelism at worst, even though the traits are `Send`-bounded.

**Fix**: a plain `#[test]` or `fn main`. `SimulationBuilder::run()` drives its own executor. Standalone code uses `Executor::new(seed)` plus `executor.block_on(..)`, as [The Deterministic Executor](../part2-foundations/11-executor.md) shows. `LocalSet`, `spawn_local` and `build_local` are gone from the model.

## Unordered Collections

**Symptom**: a seed fails once in ten runs, or the canary reports `Diverged`.

**Cause**: `HashMap` and `HashSet` iterate in an order that varies between runs. If that order picks which account to process or which message to send, the seed no longer controls the schedule.

**Fix**: `BTreeMap` and `BTreeSet`, or collect into a `Vec` and sort. The workspace's `clippy.toml` lists both hash types as disallowed, so this class of leak fails CI.

## A Variable Number of Draws per Step

**Symptom**: an exploration recipe replays to a different timeline than the one that found the bug.

**Cause**: a loop that resamples "until the operation is enabled" consumes a different number of draws on each seed. Exploration anchors its breakpoints on the RNG call count, so the anchor lands in a different place.

**Fix**: draw once and fold the result into the enabled subset, as [Swarm the Alphabet](./19-designing-workloads.md#swarm-the-alphabet) shows. Keep every per-step draw count fixed.

## Metrics Steered by the Wall Clock

**Symptom**: a metric query gives different numbers for the same seed.

**Cause**: a histogram observed from `Instant::now()`. The wall clock is not deterministic.

**Fix**: record durations from `ctx.time()`. Metric values are reported, never used to steer the run, as [Application Metrics](./25-metrics.md) explains.

## Using `unwrap()`

**Symptom**: a panic that hides the assertion that would have named the real failure.

**Cause**: in simulation a panic is an uncontrolled crash of the task, not a clean error report.

**Fix**: `Result<T, E>` with `?` everywhere. For `RwLock` poison use `.expect("RwLock poisoned: prior task panicked")`, since poisoning means a prior panic already happened.

## Holding a Lock Across `.await`

**Symptom**: "future is not `Send`" pointing at the spawn site, not at the lock.

**Cause**: spawned futures must be `Send`, and a `std::sync::MutexGuard` is not.

**Fix**: scope the guard tightly. Lock, read or mutate, drop the guard, then `.await`.

```rust
// Bad: the guard lives across the await.
let guard = state.lock().expect("RwLock poisoned: prior task panicked");
network.send(&guard.payload).await?;

// Good: copy out, drop the guard, then await.
let payload = {
    let guard = state.lock().expect("RwLock poisoned: prior task panicked");
    guard.payload.clone()
};
network.send(&payload).await?;
```

## `?Send` and `!Send` Types

**Symptom**: the compiler rejects `#[async_trait(?Send)]` on a `Process`, or refuses a spawned future that captures an `Rc` or `RefCell`.

**Cause**: `Process`, `Workload` and `FaultInjector` carry `Send + Sync + 'static` supertraits so customer code can share state through `Arc<RwLock<..>>` and `DashMap`.

**Fix**: plain `#[async_trait]` on dyn-stored impls. `Arc<RwLock<..>>` or `Arc<AtomicX>` for shared state. Provider traits use native `async fn` with `-> impl Future<..> + Send` and need no attribute.

## Delayed Operations Need the Scheduler

**Symptom**: a low-level provider test awaits a storage write or a network connect and hangs.

**Cause**: storage read, write, sync and set-length return `Pending` and complete through a scheduled event. Bind, connect and accept are delayed too. A test that polls the future without stepping `SimWorld` never completes it.

**Fix**: prefer `SimulationBuilder::run()`, which interleaves executor drains and scheduler steps. For a provider test, drive the future and the world together on `Executor::block_on` with the `drive` helper from the root `AGENTS.md`, as [Storage Faults](./11-storage-faults.md) shows.

## Awaiting `until_stalled()` From a Task

**Symptom**: a spawned task awaits `executor::until_stalled()` and the run wedges.

**Cause**: `until_stalled()` is the **driver's** drain: it waits until every runnable task has run, including the caller. The orchestrator uses it after each `sim.step()`.

**Fix**: a task that needs to yield awaits `ctx.task().yield_now()`. Leave `until_stalled()` to the driver loop.

## State Changes Without Events

**Symptom**: an invariant misses a bug, or reports a violation the workload never saw.

**Cause**: invariants read plain `tracing` events captured into the timeline. A state change with no event is invisible to them.

**Fix**: emit an `INFO` event with a constant message for every change an invariant must see. The same line serves production observability.

```rust
self.model.record_commit(slot, value);
tracing::info!(slot, value, "commit");
```

## Moving Resource State Back Into the World

**Symptom**: a borrow checker fight inside `SimWorld`, or a waker that re-enters a component under the world lock.

**Cause**: `SimWorld` coordinates the scheduler and the lifecycle. Network state belongs to `NetworkSimulation`, storage state to `StorageEngine`. Reaching through `SimInner` to mutate a resource and schedule directly couples ownership again.

**Fix**: add a targeted component transition that returns ordered schedule and cancel effects, fault records and a `WakeBatch`. The world applies them and wakes after releasing its lock. Never wake while holding it: a waker may poll immediately and re-enter the same component.
