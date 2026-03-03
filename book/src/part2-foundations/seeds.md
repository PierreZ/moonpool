# Seed-Driven Reproducibility

<!-- toc -->

One number. A single `u64`. That is all moonpool needs to fully determine a simulation: which connections fail, when packets arrive, whether BUGGIFY triggers, how long disk writes take, what order events process. Same seed, same execution, same bugs. This is the property that makes everything else work.

## The RNG Core

Moonpool uses `ChaCha8Rng` from the `rand_chacha` crate, seeded from the `u64` value. ChaCha8 is fast, produces high-quality randomness, and is deterministic across platforms. The RNG lives in thread-local storage, which is correct because each simulation runs on a single thread.

```rust
// At the start of every simulation run
set_sim_seed(seed);

// Every decision that needs randomness goes through these
let latency: u64 = sim_random_range(1..50);  // network delay in ms
let should_fail: bool = sim_random_f64() < 0.25;  // 25% fault probability
let value: f64 = sim_random();  // general-purpose random value
```

The functions `sim_random()`, `sim_random_range()`, and `sim_random_f64()` all draw from the same thread-local RNG. This means the **order of calls matters**. Adding a new `sim_random()` call anywhere in the simulation shifts every subsequent random value. This is intentional. It means small code changes produce different simulation trajectories, naturally exploring new parts of the state space.

## Call Count Tracking

Every RNG call increments a thread-local counter. You can read it with `get_rng_call_count()`. This sounds mundane, but it is one of the most useful debugging tools in the framework.

When a seed produces a bug, you can narrow down exactly where the execution diverges from expected behavior by watching the call count. "The bug triggers after RNG call 847" tells you precisely which decision in the simulation started the chain of events leading to failure. Combined with breakpoints and logging, this turns a mysterious distributed failure into a step-through debugging session.

The explorer framework takes this further: it records call counts at fork points, creating a "recipe" of `count@seed` transitions that can replay an exact exploration path.

## Multi-Seed Testing

A single seed tests one execution path. To build confidence, you need many paths. Moonpool's builder supports two modes:

**FixedCount** runs a set number of iterations, each with a different random seed:

```rust
SimulationBuilder::new()
    .set_iterations(100)  // 100 different seeds
    // ... workloads ...
    .run();
```

**TimeLimit** runs for a wall-clock duration, burning through as many seeds as time allows:

```rust
SimulationBuilder::new()
    .set_time_limit(Duration::from_secs(300))  // 5 minutes of exploration
    // ... workloads ...
    .run();
```

The power of seed-driven testing compounds over time. Run 1,000 seeds in CI on every commit. Run 100,000 overnight. Each seed explores a different combination of timing, faults, and ordering. Bugs that require three independent unlikely events to coincide will surface within a few thousand seeds because the simulation amplifies failure probability through BUGGIFY.

## Debugging a Failing Seed

When CI reports a failure, the output includes the seed:

```
FAILED seed=17429853261 — connection timeout during leader election
```

Pin that seed and run it locally with logging enabled:

```rust
SimulationBuilder::new()
    .set_iterations(1)
    .set_debug_seeds(vec![17429853261])
    // ... same workloads ...
    .run();
```

The simulation replays the exact same execution. Set a breakpoint. Step through. The bug is deterministic now.
