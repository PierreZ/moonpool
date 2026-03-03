# The Provider Pattern

<!-- toc -->

Every distributed system does five things: it talks over the network, it reads the clock, it spawns concurrent tasks, it generates random values, and it reads and writes files. That is the entire surface area where non-determinism leaks in. Moonpool's provider pattern seals all five.

## The Core Idea

Define a trait for each category of I/O. Implement the trait twice: once backed by real tokio calls for production, once backed by a deterministic simulation engine for testing. Your application code is generic over the trait. It never knows which implementation it is running against.

```text
                  Application Code
           (generic over Providers trait)
                        |
            +-----------+-----------+
            |                       |
      TokioProviders          SimProviders
     (real TCP, real         (simulated TCP,
      clock, real disk)       logical clock,
                              fault-injected disk)
```

This is **interface swapping**, the same technique FoundationDB used with their `INetwork` interface (production `Net2` vs. simulation `Sim2`). The difference is that Rust's type system enforces it at compile time. If your code compiles with `P: Providers`, it works with both implementations. No runtime surprises.

## The Providers Bundle

Carrying five separate type parameters through every function signature would be painful:

```rust
// Nobody wants to write this
fn run_server<N, T, K, R, S>(net: N, time: T, task: K, rand: R, storage: S)
where
    N: NetworkProvider, T: TimeProvider, K: TaskProvider,
    R: RandomProvider, S: StorageProvider,
{ /* ... */ }
```

Moonpool solves this with a single bundle trait called `Providers`:

```rust
pub trait Providers: Clone + 'static {
    type Network: NetworkProvider + Clone + 'static;
    type Time: TimeProvider + Clone + 'static;
    type Task: TaskProvider + Clone + 'static;
    type Random: RandomProvider + Clone + 'static;
    type Storage: StorageProvider + Clone + 'static;

    fn network(&self) -> &Self::Network;
    fn time(&self) -> &Self::Time;
    fn task(&self) -> &Self::Task;
    fn random(&self) -> &Self::Random;
    fn storage(&self) -> &Self::Storage;
}
```

Now your code carries one type parameter:

```rust
fn run_server<P: Providers>(providers: P) {
    let time = providers.time().clone();
    let net = providers.network().clone();
    // Use them naturally
}
```

Two implementations exist: `TokioProviders` for production, and `SimProviders` (in moonpool-sim) for simulation. Your application code never imports either one directly. It only sees `P: Providers`.

## One Line Changes Everything

The swap between "testing a real distributed system" and "testing inside a deterministic simulation" happens at the call site, not inside your application logic:

```rust
// Production
let providers = TokioProviders::new();
run_server(providers);

// Simulation (inside the simulation builder)
let providers = sim.providers();  // SimProviders
run_server(providers);
```

Same `run_server`. Same code path. Same binary. The only difference is which `Providers` implementation gets plugged in. This is the architectural foundation that makes everything else in moonpool possible: chaos testing, assertion coverage, multiverse exploration, all of it rests on the guarantee that your production code runs unmodified inside the simulator.
