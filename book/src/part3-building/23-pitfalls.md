# Common Pitfalls

<!-- toc -->

A reference list of mistakes we have seen (and made) when building simulations with Moonpool.

## Storage Needs the Step Loop

Network operations buffer data and return `Poll::Ready` immediately. Storage operations return `Poll::Pending` and wait for the simulation to process them. If you `await` a storage operation without stepping the simulation, your workload hangs forever.

**Fix**: Use the step loop pattern for storage tests:

```rust
let handle = tokio::task::spawn_local(async move {
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

Spawned tasks via `spawn_local` do not run until the current task yields. If your workload spawns a task and immediately checks its result without yielding, the task has never run.

**Fix**: Call `tokio::task::yield_now().await` after spawning, and in loops where you wait for spawned tasks to complete.

## Using `unwrap()`

Moonpool follows a strict no-`unwrap()` policy. In simulation, a panic from `unwrap()` is not a clean error report. It is an uncontrolled crash that may mask the real failure and confuse the assertion system.

**Fix**: Use `Result<T, E>` with `?` everywhere. Map errors with context when needed.

## Direct Tokio Calls

Calling `tokio::time::sleep()`, `tokio::time::timeout()`, or `tokio::spawn()` bypasses the simulation's control of time and task scheduling. Your code will use real wall-clock time instead of simulated time, and the simulation cannot inject faults.

**Fix**: Use provider traits: `time.sleep()`, `time.timeout()`, `task_provider.spawn_task()`.

## Using `LocalSet`

The `tokio::task::LocalSet` runtime conflicts with Moonpool's simulation engine.

**Fix**: Use `tokio::runtime::Builder::new_current_thread().build_local()` only.

## Missing `#[async_trait(?Send)]`

Moonpool runs on a single thread. All types are `!Send`. If you derive `#[async_trait]` without the `(?Send)` bound, the compiler will require `Send` on your futures.

**Fix**: Always use `#[async_trait(?Send)]` for networking traits.

## Borrow Checker Fights in `world.rs`

When working on simulation internals, you may need to access a connection (`inner.network.connections.get_mut()`) and then schedule an event (`inner.event_queue.schedule()`). The borrow checker sees both as borrows of `inner`.

**Fix**: Extract values from the connection into local variables before calling functions that take `&mut SimInner`. NLL allows the borrow of `conn` to end before you borrow `inner` again, as long as you do not use `conn` after the second borrow begins.

## HashMap Iteration Non-Determinism

`std::collections::HashMap` does not guarantee iteration order, and the order can vary between runs. If your workload iterates a `HashMap` and the iteration order affects behavior (choosing which account to process, which message to send), you have introduced non-determinism.

**Fix**: Use `BTreeMap` when iteration order matters, or collect into a `Vec` and sort before iterating.

## Forgetting to Publish State for Invariants

Invariants read from `StateHandle`. If your workload modifies its model but forgets to call `ctx.state().publish(...)`, invariants see stale data and either miss bugs or report false violations.

**Fix**: Publish state after every mutation, not just at the end.

```rust
self.model.deposit(&account, amount);
ctx.state().publish("banking_model", self.model.clone());
```
