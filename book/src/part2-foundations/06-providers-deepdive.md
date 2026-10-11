# Deep Dive: Why Providers Exist

<!-- toc -->

Providers are not the first idea a team reaches for. Most teams start with `#[cfg(test)]` or a mock framework. Both have a fundamental problem that providers solve.

## Why Not `#[cfg(test)]`

Conditional compilation is tempting. Keep the real call in production, swap a fake in during tests:

```rust,ignore
#[cfg(not(test))]
async fn connect(addr: &str) -> io::Result<TcpStream> {
    TcpStream::connect(addr).await
}

#[cfg(test)]
async fn connect(addr: &str) -> io::Result<TcpStream> {
    FakeStream::new()  // returns at once, no network
}
```

The problem: **you no longer test your production code**. The test binary compiles a different function. Every `#[cfg(test)]` block is a fork where production and test behaviour can diverge in silence. Oxide's engineers documented this across five large repositories: `#[cfg(test)]` blocks "prevent testing real code paths". The tested code is not the shipped code.

Providers remove the fork. There is one `connect()` in your application. It calls `providers.network().connect()`. In production that dispatches to tokio. In simulation it dispatches to the simulator. The application code is identical, down to the compiler output for the generic function.

## Why Not Mocks

A mock framework like `mockall` records expectations: "this method is called with these arguments and returns this value". A mock tests that your code calls the right method in the right order. It does not test what happens when the network does something unexpected.

A mock for TCP says: "when `connect("10.0.1.1:9000")` is called, return `Ok(stream)`". That says nothing about a connection that takes 3 seconds, a connection that succeeds and then resets on the first read, or a peer that rebooted between two requests and lost its state.

**Providers are full implementations, not recorded expectations.** The simulated network provider keeps connection state, buffers bytes, injects delays, models TCP half-close, and drops a connection on partition. It is a complete in-memory TCP. When your code connects through it, it gets a real (simulated) session with real (simulated) failure modes.

That fidelity includes asynchronous establishment. Simulated `bind`, `connect` and `accept` do not hand back a ready socket. They park the calling future, schedule a network operation with a delay, and wake the future when the delay expires. Storage follows the same contract for `read_at`, `write_at`, `sync_all` and `set_len`: each returns `Pending` and completes through a scheduled storage event. A provider is an adapter into a stateful engine, not a shortcut around the scheduler.

Oxide's codebases show a fidelity spectrum: no-op fakes that return `Ok(())`, in-memory implementations that hold real data without real I/O, and full protocol simulation. Providers sit at the top of that spectrum, because a simulation must hit the edge cases production hits.

## Never More Permissive

A simulated provider has one more duty: it must never accept what the production provider refuses. If the simulator accepted an open that a real filesystem rejects, simulated code would come to depend on it, and the run would stay green until it reached a real disk.

Two examples from the storage provider. `OpenOptions::validate` applies `std::fs::OpenOptions`' own rules on both backends: `truncate`, `create` and `create_new` need `write` or `append`, and `append` with `truncate` contradict each other. The simulator refuses those with `InvalidInput` before it touches a file. And `DirectIo::Required` opens an **existing** file. A `Required` open that would have to create the file is refused by both backends. `crates/moonpool-sim/tests/storage/parity.rs` runs the same scenarios through `TokioStorageProvider` and `SimStorageProvider` and asserts that they answer alike. The simulated `list_dir` is checked against tokio's for the same tree.

## The Pattern

The recipe is short. Oxide's teams converged on it independently, with almost no mock-framework use:

1. **Define a trait** for the external dependency.
2. **Implement it for production** with real tokio calls.
3. **Implement it for simulation**, deterministic and controllable.
4. **Inject it through generics**: compile-time dispatch, zero runtime overhead.

```rust,ignore
// Step 1: the trait (native async fn in trait, no #[async_trait])
pub trait TimeProvider: Clone + Send + Sync + 'static {
    fn sleep(&self, duration: Duration) -> impl Future<Output = Result<(), TimeError>> + Send;
    fn now(&self) -> Duration;
    // ...
}

// Step 2: production
impl TimeProvider for TokioTimeProvider {
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError> {
        tokio::time::sleep(duration).await;  // a real wall-clock delay
        Ok(())
    }
    // ...
}

// Step 3: simulation (moonpool-sim). sleep() schedules a timer event and
// parks the future; the scheduler advances logical time to the timer.

// Step 4: inject through generics
async fn heartbeat_loop<T: TimeProvider>(time: T) {
    loop {
        send_heartbeat().await;
        time.sleep(Duration::from_secs(5)).await.ok();
    }
}
```

If `heartbeat_loop` compiles with `T: TimeProvider`, it works with both `TokioTimeProvider` and `SimTimeProvider`. No runtime dispatch, no dynamic casts, no "forgot to wire the mock" surprise.

## Why Static Dispatch

The provider traits are not object-safe, on purpose: the `Clone` supertrait, the generic `random<T>()`, the `impl Future` return types and the associated stream types all rule out `dyn Providers`. Simulation is test-only, so no production code ever needs to pick an implementation at runtime. Where generics hurt, erase at an application boundary with a narrow trait of your own. moonpool-rpc and moonpool-hyper do exactly this: they are generic over `P: Providers` all the way down and ship no providers of their own.

## What You Get

**Same code path.** Your production code is your test code. No divergence, no conditional compilation.

**Full behaviour, not call verification.** A simulated network partitions, delays, reorders and resets. A simulated disk tears writes, corrupts sectors and lies about syncs. A mock records calls.

**Resource ownership stays local.** The network and storage engines own their state, pending results, faults and wakers. `SimWorld` coordinates their effects through one scheduler. Cancellation and shutdown are explicit: a crash completes pending work with errors, and a missing result is never mistaken for success.

**A compile-time boundary.** If you call tokio directly, the code still compiles and runs, but it is non-deterministic. Moonpool's conventions, enforced in review, catch these: no `tokio::time::sleep()`, no `tokio::spawn()`, no `tokio::net::*`, no `tokio::select!`. Always go through the provider. [Sim Compatibility Checklist](../appendix/06-sim-compatibility.md) is the full list.
