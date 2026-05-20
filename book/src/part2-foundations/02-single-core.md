# The Single-Core Constraint

<!-- toc -->

Moonpool runs every simulation on a single OS thread. This is not a limitation we reluctantly accept. It is the first design decision we made, and every other decision follows from it.

## One Thread, One Order

A multi-threaded tokio runtime uses a work-stealing thread pool. When a task yields at an `.await`, any thread in the pool might pick it up. Two tasks that resume "at the same time" can execute in either order, and that order changes between runs. This is fine for production throughput. It is fatal for deterministic simulation.

With a single thread, there is exactly one legal execution order for any given set of ready tasks. Task A resumes before Task B, or Task B before Task A, and the choice is controlled by our scheduler, not the OS. The RNG picks the order. The seed determines the RNG. Done.

## How We Get There

Moonpool uses tokio's **current-thread runtime**:

```rust
tokio::runtime::Builder::new_current_thread()
    .enable_io()
    .enable_time()
    .build()
```

One OS thread drives the entire simulation. No work-stealing pool. No preemption between yield points. When a future resumes, it resumes on the same thread that suspended it, in an order our scheduler chose deterministically from the seed.

This is the **whole** mechanism. The runtime gives us determinism. Nothing fancier is needed.

## Send-Bounded Traits

Here is where moonpool diverges from earlier designs. A single-threaded runtime **could** drop `Send` bounds entirely and let users sprinkle `Rc<RefCell<T>>` across `.await` points. We deliberately do not.

Provider traits use native AFIT with `Send + Sync + 'static` supertraits and futures that return `impl Future<…> + Send`:

```rust
pub trait TimeProvider: Clone + Send + Sync + 'static {
    fn sleep(&self, duration: Duration) -> impl Future<Output = Result<(), TimeError>> + Send;
    fn now(&self) -> Duration;
    // ...
}
```

Dyn-stored traits (`Process`, `Workload`, `FaultInjector`, `#[service]` handlers) use `#[async_trait]` without `?Send`, also with `Send + Sync + 'static` supertraits.

**Send is paid for interoperability, not for parallelism.**

The runtime still runs on one thread. Futures and types are never actually moved between threads at runtime. But the **type system** commits to Send + Sync at every trait boundary so customer code can use the ecosystem it already knows:

- `Arc<RwLock<State>>` for shared state
- `DashMap` for concurrent maps
- `Arc<AtomicBool>` for flags
- `tokio::spawn` to fan out work

A team adopting moonpool to test an existing service should not have to rewrite their type hierarchy. If their production code uses `Arc<RwLock<…>>` (it does), moonpool wraps it without contortions. The simulation runtime drives those types on one thread, but the types themselves look exactly like the production ones.

## What This Buys, What It Costs

We get determinism from the runtime and ergonomics from the bounds. The combination is rare. Most deterministic simulators force you to pick: either a single-threaded runtime with `!Send` types that don't compose with anything, or a multi-threaded runtime that loses determinism.

The cost is a small one. `Send + Sync` constrains a few exotic patterns (raw `Rc`, non-`Send` futures from external crates). In a year of building on this, we have not hit a case where it mattered. The patterns that **do** matter — locks, channels, spawned tasks — are all Send-friendly already.

## The Parallelism Trade-Off

Single-core simulation cannot exploit CPU parallelism **within** a simulation run. A simulation of 20 nodes runs on one core, not 20.

In practice, this barely matters. Simulation time compression means a single core simulates hundreds of seconds of cluster time in a few seconds of wall-clock time. And we run **many simulations in parallel** across cores, each with a different seed. A 16-core machine runs 16 independent simulations concurrently, each fully deterministic on its own thread.

Production code is unaffected. The same code that runs single-threaded under our simulation runtime can run on a multi-threaded tokio runtime in production. The provider pattern (covered next) makes this swap transparent.
