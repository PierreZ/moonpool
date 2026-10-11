# From Mocks to Simulation

<!-- toc -->

Every experienced developer has a mock story. You spend a day writing mocks for a database client. You specify which method returns what, in which order. The tests pass. Then someone refactors the internal call sequence without changing any external behavior, and every mock breaks. The mocks were not testing correctness. They were testing implementation details.

This is not the fault of any mocking library. It is a structural problem with how mocks work.

## Why mocks fail at scale

A mock replaces a dependency with a fake that returns pre-programmed responses. To program those responses, you must know which methods your code calls, in which order, with which arguments. The test author carries a mental model of the whole call stack between the code under test and the mocked dependency.

For a unit test of one function, that is manageable. For an integration test of a distributed protocol with concurrent operations, retries, timeouts and failure handling, it is a maintenance nightmare. Every internal refactor breaks mocks that tested behavior. Every new failure path needs new mock responses. The mock setup grows until it rivals the system it tests.

Mocks also skip layers. Production code talks to a TCP socket. The mock replaces the database client. Between those two layers live connection pooling, serialization, retry logic, timeout handling and error translation. None of that code runs in a mock-based test. You test a different system from the one you deploy.

## The `#[cfg(test)]` trap

Rust developers often reach for conditional compilation. `#[cfg(test)]` swaps in a test-only implementation with no runtime cost and no trait indirection. But the binary you test is now a different binary from the one you deploy. Different code paths, different struct fields, different behavior.

Suppose a bug lives in the interaction between your retry logic and your connection pool. Your test build replaces the pool with an in-memory stub under `#[cfg(test)]`. The bug is invisible to the test suite. You have not tested the system. You have tested a system-shaped thing that shares some code with it.

The same trap has a second form: a `SimMachine` next to `Machine`, a `SimDisk` next to `Disk`, a `#[cfg(test)]` hook that lets the test reach inside. Each one is a second implementation that the tests exercise and production never runs.

## The alternative: trait-based simulation

There is a different approach. Define a **trait** that describes the interface your code needs. Implement it once for production (real TCP, real disk, real clock). Implement it once for simulation (simulated network, simulated disk, simulated clock). Application code depends on the trait. The simulated implementation runs **real logic**, not canned responses, so every path production exercises, simulation exercises too.

```rust
pub trait TimeProvider: Clone + Send + Sync + 'static {
    fn sleep(&self, duration: Duration)
        -> impl Future<Output = Result<(), TimeError>> + Send;

    fn now(&self) -> Duration;
}
```

In production, `sleep` calls `tokio::time::sleep`. In simulation, `sleep` registers a timer with the simulated scheduler, and logical time jumps forward when every task is blocked. The application code is identical. No `#[cfg(test)]`. No conditional compilation.

This is the **provider pattern**. FoundationDB made the same decision with its `INetwork` interface: one interface, two implementations (`Net2` for production, `Sim2` for simulation), zero conditional logic in the database.

## The fidelity spectrum

Not every simulated implementation needs full fidelity. There is a spectrum.

**No-op.** `sleep` returns at once, `send` discards the message. Useful for pure logic that happens to call I/O.

**In-memory.** Messages go into a queue, disk writes into a `BTreeMap`. Fast and deterministic, but no timing, no failures, no reordering.

**Controlled simulation.** Messages are delayed by random amounts, connections drop on a fault schedule, disk writes can be torn or corrupted. This is where bugs hide, because the system must handle every way the real world deviates from the happy path. These implementations scale: you write each one once, and every component that uses the trait gets simulation for free. No per-test setup.

**Full simulation.** A whole cluster of processes with simulated topology, coordinated fault injection and time compression. FoundationDB runs hundreds of simulated processes on one thread and compresses hours of cluster behavior into seconds.

Pick the level by what you test. A serialization function needs no-op I/O. A retry loop needs controlled failure injection. A consensus protocol needs full cluster simulation.

## Injected conditions over expected calls

The deepest difference between mocks and simulation is the direction of control.

A mock **specifies outputs**: "when this method is called with these arguments, return this value." The author must predict every call. If the code takes another path, the mock panics.

A simulation **injects conditions**: "connections close with this probability, connects take 1 to 11 ms, a sector written since the last sync may be lost after a crash." The simulation does not care which methods run or in which order. It cares whether the system **recovers correctly** whatever happens.

Mock-based tests verify that code follows a specific path. Simulation-based tests verify that code produces correct results on every path the simulation explores. One tests implementation. The other tests behavior.

## Where the code names a moment

There is one place where the code under test does talk to the simulator, and it is worth being precise about it. A trait swap cannot know that "the batch is durable but its messages are not sent yet" is an interesting instant. Only the code knows that.

Moonpool lets shipped code name such a moment with `hint!("batch durable, not sent").await`. The code never names a fault. In production the hint is a no-op. In simulation the simulator decides whether to strike there and how, from the fault families the seed enabled. The code's own rare-but-legal choices use `buggify!`: an early timeout, a small buffer, a slow path. Both are inert outside a simulation, so the shipped binary carries them at no cost. See [hint!: Naming Fault Sites](../part3-building/29-hints.md) and [Buggify](../part3-building/08-buggify.md).

## Moonpool's provider pattern

This is what moonpool implements. Every interaction with the outside world goes through one of six traits in `moonpool-core`: `TimeProvider` for clocks and timers, `TaskProvider` for spawning, `NetworkProvider` for connections, `StorageProvider` for files, `RandomProvider` for randomness, `SystemProvider` for CPU and disk counters. The `Providers` trait bundles them, and `TokioProviders` is the production bundle.

Your code calls `time.sleep()` instead of `tokio::time::sleep()`, and `task.spawn_task()` instead of `tokio::spawn()`. In production these call the real runtime. In simulation they feed a deterministic scheduler where every timer, message and disk operation is controlled, reproducible and open to fault injection.

No mocks to maintain. No `#[cfg(test)]` to split your binary. The same code, in a world that is deliberately worse than reality. Part II covers the traits in [The Six Providers](../part2-foundations/07-provider-traits.md) and the rule behind them in [Same Code in Production and Simulation](../part2-foundations/12-same-code.md).
