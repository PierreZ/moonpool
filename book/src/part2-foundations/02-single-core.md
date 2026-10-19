# The Single-Core Constraint

<!-- toc -->

Moonpool runs every simulation on one OS thread. This is not a limitation we accept with regret. It is the first design decision, and every other decision follows from it.

## One Thread, One Order

A multi-threaded tokio runtime uses a work-stealing thread pool. When a task yields at an `.await`, any thread in the pool can pick it up. Two tasks that resume "at the same time" run in either order, and the order changes between runs. This is fine for production throughput. It is fatal for a simulation that must replay.

With one thread there is exactly one execution order for a given set of ready tasks. Task A runs before task B, or B before A, and our scheduler makes the choice, not the OS. The RNG picks the order. The seed determines the RNG. Done.

## How We Get There

Every simulation runs on the moonpool **deterministic executor**:

```rust,ignore
moonpool_sim::reset_sim_rng();
moonpool_sim::set_sim_seed(seed);

let mut executor = moonpool_sim::executor::Executor::new(seed);
executor.block_on(async move {
    // the entire simulation runs here
});
```

One OS thread drives the whole simulation. There is no work-stealing pool and no preemption between yield points. When a future resumes, it resumes on the thread that suspended it. Which ready task runs **next** is a seeded draw from the ready queue. The same seed gives the same order, bit for bit. A different seed explores a different interleaving. A FIFO scheduler would run ready tasks in registration order on every seed, forever, and would hide every race between them. [The Deterministic Executor](./11-executor.md) covers the mechanism.

There is **no tokio runtime inside a simulation**. A bare `tokio::spawn` panics at runtime, because there is no runtime to spawn onto. Spawn through `ctx.task().spawn_task()` or `moonpool_sim::executor::spawn()`. Sleep through `ctx.time().sleep()`. Select with `moonpool_sim::select!`.

## Send-Bounded Traits

Here moonpool diverges from earlier single-threaded designs. A single-threaded runtime **could** drop `Send` bounds and let you put `Rc<RefCell<T>>` across `.await` points. We deliberately do not.

Provider traits use native async-fn-in-trait with `Send + Sync + 'static` supertraits, and every async method returns `impl Future<…> + Send`:

```rust,ignore
pub trait TimeProvider: Clone + Send + Sync + 'static {
    fn sleep(&self, duration: Duration) -> impl Future<Output = Result<(), TimeError>> + Send;
    fn now(&self) -> Duration;
    // ...
}
```

The traits the runner stores as `dyn` objects (`Process`, `Workload`, `FaultInjector`) use `#[async_trait]` without `?Send`, with the same `Send + Sync + 'static` supertraits.

**We pay for `Send` to get interoperability, not parallelism.**

The runtime still runs on one thread. No future ever moves between threads at runtime. But the **type system** commits to `Send + Sync` at every trait boundary, so your code can use the ecosystem it already knows:

- `Arc<RwLock<State>>` for shared state
- `DashMap` for concurrent maps
- `Arc<AtomicBool>` for flags
- `Send`-bounded spawning: `ctx.task().spawn_task("name", fut)` in the sim, `tokio::spawn` in production

A team that adopts moonpool to test an existing service should not rewrite its type hierarchy. If the production code uses `Arc<RwLock<…>>` (it does), moonpool runs it as is. The simulation drives those types on one thread, but the types look exactly like production.

There is no tokio `LocalSet`, no tokio `spawn_local` and no `build_local` inside a simulation. Spawned futures are `Send + 'static`, as `tokio::spawn` requires.

One exception is additive. A `!Send` task, one that holds `Rc` or `RefCell`, spawns through `ctx.task().spawn_local(name, fut)`. It runs on the same executor and the same seeded queue, so determinism does not change. Thread-per-core production runtimes (compio) implement the same `LocalTaskProvider`. The `Send` contract on every trait stays: we pay for it to get interoperability, and a local task simply does not spend it.

## What This Costs

`Send + Sync` rules out a few patterns: raw `Rc`, and non-`Send` futures from external crates. In practice the patterns that matter (locks, channels, spawned tasks) are already `Send`-friendly.

The other cost is CPU parallelism **inside** one run. A simulation of 20 nodes runs on one core, not 20. This rarely matters. Simulated time is compressed: one core simulates hours of cluster time in seconds of wall-clock time, because the scheduler jumps straight to the next event. And we run **many simulations in parallel** across cores, each with its own seed. A 16-core machine runs 16 independent simulations at once, each deterministic on its own thread.

Production code is unaffected. The same code that runs single-threaded in the simulation runs on a multi-threaded tokio runtime in production. The provider pattern, covered next, makes that swap invisible to the code.
