# The Single-Core Constraint

<!-- toc -->

Moonpool runs every simulation on a single thread. This is not a limitation we reluctantly accept. It is the first design decision we made, and every other decision follows from it.

## One Thread, One Order

A multi-threaded tokio runtime uses a work-stealing thread pool. When a task yields at an `.await`, any thread in the pool might pick it up. Two tasks that resume "at the same time" can execute in either order, and that order changes between runs. This is fine for production throughput. It is fatal for deterministic simulation.

With a single thread, there is exactly one legal execution order for any given set of ready tasks. Task A resumes before Task B, or Task B before Task A, and the choice is controlled by our scheduler, not the OS. The RNG picks the order. The seed determines the RNG. Done.

## How We Get There

Moonpool uses tokio's **single-threaded, local runtime**:

```rust
tokio::runtime::Builder::new_current_thread()
    .enable_io()
    .enable_time()
    .build_local(Default::default())
```

This is **not** a `LocalSet` on top of a multi-threaded runtime. `build_local` creates a true single-threaded runtime where `spawn_local` is the only spawn mechanism. No `Send` bounds. No `Sync` bounds. No possibility of cross-thread data races.

This means every type in simulation can use `Rc`, `RefCell`, raw pointers, thread-local storage, whatever is needed. No `Arc<Mutex<>>` overhead. No `Send` bounds propagating through your entire type hierarchy.

## The ?Send Constraint

All networking traits in moonpool use `#[async_trait(?Send)]`:

```rust
#[async_trait(?Send)]
pub trait TimeProvider: Clone {
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError>;
    fn now(&self) -> Duration;
    // ...
}
```

The `?Send` bound tells Rust that futures produced by these traits do not need to be `Send`. This is correct because we never move them between threads. It also means your async code can hold `Rc<RefCell<T>>` across `.await` points, which is impossible with `Send`-requiring runtimes.

## The Trade-Off

Single-core simulation cannot exploit CPU parallelism **within** a simulation run. A simulation of 20 nodes runs on one core, not 20.

In practice, this barely matters. Simulation time compression means a single core simulates hundreds of seconds of cluster time in a few seconds of wall-clock time. And you can run **many simulations in parallel** across cores, each with a different seed. A 16-core machine runs 16 independent simulations concurrently, each fully deterministic on its own thread.

Your production code is unaffected. The same code that runs single-threaded in simulation can run on a multi-threaded tokio runtime in production. The provider pattern (covered next) makes this swap transparent.
