# The Provider Pattern

<!-- toc -->

Every distributed system does five things with the outside world: it reads the clock, it spawns concurrent tasks, it talks over the network, it draws random numbers, and it reads and writes files. That is the whole surface where non-determinism leaks in. Moonpool seals all five behind traits, and adds a sixth trait that reads how busy the process and its disk are, so that measurement runs in simulation too.

## The Core Idea

Define a trait for each category of I/O. Implement the trait twice: once over real runtime calls for production (tokio, or compio for thread-per-core), once over the deterministic world for testing. Your application code is generic over the trait. It never knows which implementation it runs against.

```text
                  Application Code
             (generic over P: Providers)
                        |
            +-----------+-----------+
            |                       |
      TokioProviders          SimProviders
     (real TCP, wall         (simulated TCP,
      clock, real disk,       logical clock,
      OS entropy)             fault-injected disk,
                              seeded RNG)
```

This is **interface swapping**, the technique FoundationDB uses with its `INetwork` interface: `Net2` in production, `Sim2` in simulation. The difference is that Rust enforces it at compile time. If your code compiles with `P: Providers`, it works with both implementations.

## The Providers Bundle

Six type parameters through every signature would be painful:

```rust,ignore
// Nobody wants to write this
fn run_server<N, T, K, R, S, Y>(net: N, time: T, task: K, rand: R, storage: S, sys: Y)
where
    N: NetworkProvider, T: TimeProvider, K: TaskProvider,
    R: RandomProvider, S: StorageProvider, Y: SystemProvider,
{ /* ... */ }
```

Moonpool bundles them in one trait, `Providers`:

```rust,ignore
pub trait Providers: Clone + Send + Sync + 'static {
    type Network: NetworkProvider;
    type Time: TimeProvider;
    type Task: TaskProvider;
    type Random: RandomProvider;
    type Storage: StorageProvider;
    type System: SystemProvider;

    fn network(&self) -> &Self::Network;
    fn time(&self) -> &Self::Time;
    fn task(&self) -> &Self::Task;
    fn random(&self) -> &Self::Random;
    fn storage(&self) -> &Self::Storage;
    fn system(&self) -> &Self::System;
}
```

Now your code carries one type parameter:

```rust,ignore
fn run_server<P: Providers>(providers: P) {
    let time = providers.time().clone();
    let net = providers.network().clone();
    // use them like tokio
}
```

Two families of implementation exist. `TokioProviders::new()` in moonpool-core is the default production bundle. `TokioLocalProviders` and `CompioProviders` are the variants for code that spawns non-`Send` tasks, covered in [Choosing a runtime](../part4-integration/05-production.md#choosing-a-runtime). `SimProviders::new(sim, ip)` in moonpool-sim is simulation: it takes a weak handle on the `SimWorld` and the IP of the process it serves, so its network, storage and system providers are scoped to that process's machine. Your application code imports neither. It sees `P: Providers` and nothing else.

Each provider in the bundle is `Clone`. Clone the one you need and move it into a spawned task, the way you would clone an `Arc`.

## One Line Changes Everything

The swap between production and simulation happens at the call site, not inside the logic:

```rust,ignore
// Production
let providers = TokioProviders::new();
run_server(providers);

// Simulation: inside a Process or Workload, the runner hands out SimProviders
let providers = ctx.providers().clone();
run_server(providers);
```

Same `run_server`. Same code path. The only difference is which `Providers` is plugged in. This is the foundation that everything else in moonpool stands on. Chaos, assertion coverage, hints, exploration: all of it rests on the guarantee that the shipped code runs unmodified inside the simulator. [Same Code in Production and Simulation](./12-same-code.md) states that rule in full.

## Thin Providers, Stateful Engines

On the simulation side each provider is deliberately thin. `SimTimeProvider::sleep` schedules a timer in the global `Scheduler<Event>`. `SimNetworkProvider::connect` asks `NetworkSimulation` for a delayed connect operation. `SimStorageProvider::open` asks `StorageEngine`. The engines own the state, the exact pending results, the faults and the wakers. They return scheduling effects to `SimWorld`, which puts them on the one timeline. The application-facing trait stays familiar, and the simulator never collapses into one global state object.

## What Stays Outside the Bundle

Name resolution is a separate trait, `Resolver`, with `TokioResolver` in production and `ScriptedResolver` in simulation. Most code never resolves a name, and code that does picks its resolver next to its providers. `moonpool_core::select!` is a macro, not a provider: it is tokio's own `select!` expansion with a seeded start offset once moonpool-sim is in the build graph. [The Six Providers](./07-provider-traits.md) lists every trait, method by method.
