# Common Pitfalls

<!-- toc -->

A reference list of mistakes we have seen (and made) when building simulations with Moonpool.

## Storage Needs the Step Loop

Network operations buffer data and return `Poll::Ready` immediately. Storage operations return `Poll::Pending` and wait for the simulation to process them. If you `await` a storage operation without stepping the simulation, your workload hangs forever.

**Fix**: Use the step loop pattern for storage tests:

```rust
let handle = tokio::spawn(async move {
    let mut file = provider.open("test.txt", OpenOptions::create_write()).await?;
    file.write_all(b"hello").await?;
    file.sync_all().await
});

while !handle.is_finished() {
    while sim.pending_event_count() > 0 {
        sim.step();
    }
    tokio::task::yield_now().await;
}
```

## Missing `yield_now()` Calls

Spawned tasks do not run until the current task yields. If your workload spawns a task and immediately checks its result without yielding, the task has never run.

**Fix**: Call `tokio::task::yield_now().await` after spawning, and in loops where you wait for spawned tasks to complete.

## Using `unwrap()`

Moonpool follows a strict no-`unwrap()` policy. In simulation, a panic from `unwrap()` is not a clean error report. It is an uncontrolled crash that may mask the real failure and confuse the assertion system.

**Fix**: Use `Result<T, E>` with `?` everywhere. Map errors with context when needed. For `RwLock` poison, use `.expect("RwLock poisoned: prior task panicked")` since poisoning means a prior panic already happened.

## Direct Tokio Calls

Calling `tokio::time::sleep()`, `tokio::time::timeout()`, or `tokio::spawn()` bypasses the simulation's control of time and task scheduling. Your code will use real wall-clock time instead of simulated time, and the simulation cannot inject faults.

**Fix**: Use provider traits: `time.sleep()`, `time.timeout()`, `task_provider.spawn_task()`.

## Wrong Runtime Flavor

Moonpool runs on a **single OS thread** for determinism. Building a `new_multi_thread()` runtime or using `#[tokio::main]` (which defaults to multi-thread) introduces real parallelism and destroys reproducibility, even though traits are `Send`-bounded.

**Fix**: Build the runtime with `tokio::runtime::Builder::new_current_thread().build()`. Do not use `LocalSet`, `spawn_local`, or `build_local()` — they are gone from the model.

## Using `?Send` on Dyn Traits

Moonpool's dyn-stored traits (`Process`, `Workload`, `FaultInjector`, `#[service]` handlers) carry `Send + Sync + 'static` supertraits so customer code can share state through `Arc<RwLock<…>>` and `DashMap`. Writing `#[async_trait(?Send)]` on your impl removes the `Send` bound the trait promises and the compiler rejects it.

**Fix**: Use plain `#[async_trait]` (no `?Send`) on dyn-stored impls. Provider traits use native AFIT with `-> impl Future<…> + Send` and need no attribute at all.

## Holding a Lock Across `.await`

Because spawned futures must be `Send`, any local you hold across an `.await` point must also be `Send`. A `std::sync::MutexGuard` is `!Send`, so holding one across an await fails to compile with a confusing "future is not `Send`" error pointing at the spawn site, not the lock.

**Fix**: Scope the guard tightly. Lock, read or mutate, drop the guard, then `.await`. Prefer `parking_lot::Mutex` or `tokio::sync::Mutex` when you genuinely need to hold state across awaits — and even then, drop the guard before any long-running await.

```rust
// Bad: guard lives across the await
let guard = state.lock().expect("RwLock poisoned: prior task panicked");
network.send(&guard.payload).await?;

// Good: drop the guard first
let payload = {
    let guard = state.lock().expect("RwLock poisoned: prior task panicked");
    guard.payload.clone()
};
network.send(&payload).await?;
```

## Smuggling `!Send` Types Into Spawned Futures

`Rc`, `RefCell`, and raw pointers are `!Send`. Capturing them in a future that you hand to `tokio::spawn` or `task_provider.spawn_task()` poisons the whole future and the compiler refuses it.

**Fix**: Use `Arc<RwLock<…>>` or `Arc<AtomicX>` for shared mutable state. If you have a legitimate single-task data structure, keep it on the stack of a non-spawned future, never close over it in a spawn.

## Borrow Checker Fights in `world.rs`

When working on simulation internals, you may need to access a connection (`inner.network.connections.get_mut()`) and then schedule an event (`inner.event_queue.schedule()`). The borrow checker sees both as borrows of `inner`.

**Fix**: Extract values from the connection into local variables before calling functions that take `&mut SimInner`. NLL allows the borrow of `conn` to end before you borrow `inner` again, as long as you do not use `conn` after the second borrow begins.

## HashMap Iteration Non-Determinism

`std::collections::HashMap` does not guarantee iteration order, and the order can vary between runs. If your workload iterates a `HashMap` and the iteration order affects behavior (choosing which account to process, which message to send), you have introduced non-determinism.

**Fix**: Use `BTreeMap` when iteration order matters, or collect into a `Vec` and sort before iterating.

## Forgetting to Emit Events for Invariants

Invariants read plain `tracing` events captured into the timeline. If your workload changes state but forgets to emit a corresponding event, invariants see an incomplete history and either miss bugs or report false violations.

**Fix**: Emit a trace event for every state change you want invariants to observe. The same event also flows to production observability subscribers, so this serves a dual purpose.

```rust
self.model.record_commit(slot, value);
tracing::info!(slot, value, "commit");
```
