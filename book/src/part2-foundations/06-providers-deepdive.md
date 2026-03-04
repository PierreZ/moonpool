# Deep Dive: Why Providers Exist

<!-- toc -->

Providers are not the first idea anyone reaches for when testing distributed systems. Most teams start with `#[cfg(test)]` or mock frameworks. Both approaches have fundamental problems that providers solve.

## Why Not #[cfg(test)]

Conditional compilation is tempting. Wrap the real network call in production code, swap in a fake during tests:

```rust
#[cfg(not(test))]
async fn connect(addr: &str) -> io::Result<TcpStream> {
    TcpStream::connect(addr).await
}

#[cfg(test)]
async fn connect(addr: &str) -> io::Result<TcpStream> {
    FakeStream::new()  // returns immediately, no real network
}
```

The problem: **you are no longer testing your production code**. The test binary compiles a different function. Every `#[cfg(test)]` block is a fork in your codebase where production and test behavior can silently diverge. As Oxide Computer's engineering team documented across their five major repositories, `#[cfg(test)]` blocks "prevent testing real code paths." The tested code is not the shipped code.

Providers eliminate this entirely. There is one `connect()` implementation in your application. It calls `providers.network().connect()`. In production, that dispatches to tokio. In simulation, that dispatches to the simulator. The application code is identical in both cases. Same binary, same compiler output, same code path.

## Why Not Mocks

Mock frameworks like `mockall` record expectations: "this method should be called with these arguments and return this value." They test that your code calls the right methods in the right order. They do not test what happens when the network does something unexpected.

A mock for TCP might say: "when `connect("10.0.1.1:9000")` is called, return `Ok(stream)`." This tells you nothing about what happens when the connection takes 3 seconds, or when it succeeds but the first read returns `ConnectionReset`, or when the connection succeeds on the third retry but the remote peer has rebooted and lost state.

**Providers are full implementations, not recorded expectations.** The simulation network provider maintains connection state, buffers packets, injects delays, simulates TCP half-close, drops messages under partition. It is a complete in-memory TCP implementation. When your code connects through the simulation provider, it gets a real (simulated) TCP session with real (simulated) failure modes.

This is the fidelity spectrum that Oxide's codebases demonstrate: from no-op fakes (methods return `Ok(())`), through in-memory implementations (real data operations without real I/O), all the way to full protocol simulation. Providers operate at the highest fidelity level because simulation needs to exercise the same edge cases that production encounters.

## The Pattern

The recipe is simple. Oxide's engineering teams converged on it independently across five repositories with near-zero mock framework usage:

1. **Define a trait** for the external dependency
2. **Implement it for production** (real tokio calls)
3. **Implement it for simulation** (deterministic, controllable)
4. **Inject via generics** (compile-time dispatch, zero runtime overhead)

```rust
// Step 1: The trait
#[async_trait(?Send)]
pub trait TimeProvider: Clone {
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError>;
    fn now(&self) -> Duration;
}

// Step 2: Production implementation
impl TimeProvider for TokioTimeProvider {
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError> {
        tokio::time::sleep(duration).await;  // real wall-clock delay
        Ok(())
    }
}

// Step 3: Simulation implementation (in moonpool-sim)
// sleep() schedules a timer event, the simulator advances logical time

// Step 4: Inject via generics
async fn heartbeat_loop<T: TimeProvider>(time: T) {
    loop {
        send_heartbeat().await;
        time.sleep(Duration::from_secs(5)).await.ok();
    }
}
```

The compiler guarantees correctness: if `heartbeat_loop` compiles with `T: TimeProvider`, it works with **both** `TokioTimeProvider` and the simulation time provider. No runtime dispatch. No dynamic casts. No "forgot to wire up the mock" surprises.

## What You Get

The provider pattern gives you three things that mocks and `#[cfg(test)]` cannot:

**Same code path.** Your production code is your test code. No divergence, no conditional compilation, no "it works in test but fails in production" surprises.

**Full behavior, not just call verification.** Providers simulate the actual behavior of the subsystem, not just whether your code calls the right methods. A simulated network can partition, delay, reorder, and corrupt. A mock just records calls.

**Compile-time safety.** If you forget to use a provider and call tokio directly, the simulation still compiles and runs, but the behavior will not be deterministic. Moonpool's code conventions (enforced in review) catch these: no direct `tokio::time::sleep()`, no `tokio::spawn()`, no `tokio::net::*`. Always go through the provider.
