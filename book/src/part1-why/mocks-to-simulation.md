# From Mocks to Simulation

<!-- toc -->

Every experienced developer has a mock story. You spend a day writing mocks for a database client, carefully specifying which methods return what, in which order. The tests pass. Then someone refactors the internal call sequence without changing any external behavior, and every mock breaks. The mocks were not testing your system's correctness. They were testing its implementation details.

This is not a failure of any particular mocking library. It is a structural problem with how mocks work.

## Why mocks fail at scale

Mocks operate by replacing a dependency with a fake that returns pre-programmed responses. To program those responses, you need to know exactly which methods your code will call, in which order, with which arguments. This means the test author must carry a mental model of the entire internal call stack between the code under test and the mocked dependency.

For a unit test of a single function, this is manageable. For an integration test of a distributed protocol with concurrent operations, retries, timeouts, and failure handling, it becomes a maintenance nightmare. Every internal refactor risks breaking mocks that were testing behavior, not implementation. Every new failure path requires manually programming new mock responses. The mock setup code grows until it rivals the complexity of the system it is supposed to test.

And mocks jump abstraction layers. Your production code talks to a TCP socket. Your mock replaces the database client. Between those two layers live connection pooling, serialization, retry logic, timeout handling, and error translation. None of that code runs during mock-based tests. You are testing a different system than the one you deploy.

## The `#[cfg(test)]` trap

Rust developers often reach for conditional compilation: `#[cfg(test)]` to swap in test-specific implementations. This is tempting because it requires no runtime cost and no trait indirection. But it means the binary you test is literally different from the binary you deploy. Different code paths, different struct fields, different behavior.

If a bug lives in the interaction between your retry logic and your connection pool, and your test build replaces the connection pool with an in-memory stub via `#[cfg(test)]`, that bug is invisible to your test suite. You have not tested the system. You have tested a system-shaped thing that happens to share some code.

## The alternative: trait-based simulation

There is a different approach. Instead of replacing entire subsystems with hand-programmed fakes, define a **trait** that describes the interface your code needs. Implement it once for production (real TCP, real disk, real clock). Implement it once for simulation (simulated network, simulated disk, simulated clock). Your application code depends on the trait, not the implementation.

```rust
#[async_trait(?Send)]
pub trait TimeProvider {
    async fn sleep(&self, duration: Duration);
    fn now(&self) -> Instant;
}
```

In production, `sleep` calls `tokio::time::sleep`. In simulation, `sleep` registers a timer with the simulated event loop and advances simulated time. The application code is identical in both cases. No `#[cfg(test)]`. No conditional compilation. The exact same binary logic runs in production and in simulation.

This is the **provider pattern**. It is the same architectural decision FoundationDB made with their `INetwork` interface: one trait, two implementations, zero conditional logic in the application.

## The fidelity spectrum

Not every simulated implementation needs full fidelity. There is a spectrum.

**No-op**: the simplest fake. `sleep` returns immediately, `send` discards the message. Useful for testing pure logic that happens to call I/O functions.

**In-memory**: messages go into a queue, disk writes go into a HashMap. Fast, deterministic, but does not model timing, failures, or ordering.

**Controlled simulation**: messages are delayed by randomized amounts, connections drop according to a fault schedule, disk writes can be torn or corrupted. This is where bugs hide, because the system must handle not just the happy path but all the ways the real world deviates from it.

**Full simulation**: an entire cluster of processes with simulated network topology, coordinated fault injection, and time advancement. FoundationDB runs hundreds of simulated processes in a single thread, compressing hours of cluster behavior into seconds.

The right level depends on what you are testing. A serialization function needs no-op I/O. A retry loop needs controlled failure injection. A consensus protocol needs full cluster simulation.

## Error injection over expectations

The deepest difference between mocks and simulation is the direction of control.

Mocks **specify outputs**: "when this method is called with these arguments, return this value." The test author must predict every call. If the code takes a different path, the mock panics.

Simulation **injects conditions**: "connections drop with 5% probability, messages are delayed 1-100ms, disk writes fail 1 in 1000." The simulation does not care which methods are called or in which order. It cares whether the system **recovers correctly** regardless of which failures occur.

This is a fundamental shift. Mock-based tests verify that your code follows a specific execution path. Simulation-based tests verify that your code produces correct results across all execution paths the simulation explores. One tests implementation. The other tests behavior.

## Moonpool's provider pattern

This is exactly what moonpool implements. Every interaction with the outside world goes through a provider trait: `TimeProvider` for clocks and timers, `TaskProvider` for spawning concurrent work, `NetworkProvider` for connections and messages, `StorageProvider` for disk I/O, `RandomProvider` for randomness.

Your application code calls `time.sleep()` instead of `tokio::time::sleep()`. It calls `task_provider.spawn_task()` instead of `tokio::spawn()`. In production, these call through to the real runtime. In simulation, they feed into a deterministic event loop where every timer, every message, every disk operation is controlled, reproducible, and subject to fault injection.

No mocks to maintain. No `#[cfg(test)]` to diverge your test binary from your production binary. The same code, running in a simulated world that is deliberately worse than reality.
