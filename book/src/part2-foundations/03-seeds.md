# Seed-Driven Reproducibility

<!-- toc -->

One number. A single `u64`. That is all moonpool needs to determine a simulation in full: which connections fail, when packets arrive, which buggify sites fire, how long disk writes take, which task polls next. Same seed, same execution, same bugs. This property makes everything else work.

## The RNG Core

Moonpool uses `ChaCha8Rng` from the `rand_chacha` crate, seeded from the `u64`. ChaCha8 is fast, produces good randomness and is deterministic across platforms. The generator lives in thread-local storage. That is correct because each simulation runs on one thread, and parallel test threads never share a stream.

```rust,ignore
// The runner does this at the start of every iteration
moonpool_sim::reset_sim_rng();
moonpool_sim::set_sim_seed(seed);

// Inside the engine, every decision draws from the thread-local stream
let latency: u64 = sim_random_range(1..50);   // a message delay
let fail: bool = sim_random_bool(0.25);       // a fault coin, always one draw
let ratio: f64 = sim_random_f64();            // FDB's random01()
```

`sim_random`, `sim_random_range`, `sim_random_f64` and `sim_random_bool` are the framework's own entry points. Your application code uses `providers.random().random()` and `providers.random().random_range()` instead. The simulation's `SimRandomProvider` routes those calls to the same stream.

There is exactly **one stream**. The executor's choice of the next task, the `select!` start offset, the per-seed swarm masks, buggify activation and firing, every network, storage and CPU fault coin, and every `RandomProvider` call your code makes all draw from the same generator. Moonpool keeps no private stream for its own decisions. So the **order of draws matters**. Add one `sim_random()` call anywhere and every later value shifts. This is intended: a small code change gives a different trajectory and explores new state.

## Call Count Tracking

Every draw increments a thread-local counter, readable with `rng_call_count()`. This sounds mundane. It is one of the most useful debugging tools in the framework.

When a seed produces a bug, "the bug triggers after draw 847" tells you which decision started the chain of events. `set_rng_breakpoints(vec![(count, new_seed)])` reseeds the stream when the count passes `count`. The explorer records these `count@seed` pairs at each discovery and replays an exact path with `SimulationBuilder::replay_timeline(seed, recipe)`. Because scheduling and `select!` offsets are draws on the same stream, a recipe replays the task order along with the faults. [Multiverse Exploration](../part5-building-on-top/01-exploration.md) builds on this.

## The Determinism Canary

Everything above assumes nothing escapes the seed. Something always tries to: a `HashMap` iterated in its randomized order, a `static` that survives from one iteration into the next, a wall-clock timestamp, an OS random number from a dependency. Each one makes the simulation draw something different, or draw at a different moment, and the seed stops reproducing. Moonpool ships a canary for exactly this, the mechanism madsim's `Runtime::check_determinism` uses:

```rust,ignore
SimulationBuilder::new()
    .check_determinism()
    .workload_factory(|| Box::new(MyWorkload::new()))
    .run()?;
```

Every seed now runs twice. During the first run, each draw records a 64-bit fingerprint: a probe of the generator's state after the draw, taken on a clone so the check consumes no randomness, mixed with the logical clock. During the second run the same hook compares each draw against the record. At the end the whole record must have been consumed. Any uncontrolled difference in scheduling, draw count or simulated timing shows up as a mismatch, and the seed fails with the always-assertion `determinism canary: replay matched the recorded draw sequence`, naming the first diverging draw. A replay that stops early fails too. The replay is also judged as a full run: a workload error, an always-violation or a deadlock fails the seed even when every fingerprint matched.

Two things to know. It is a canary, not a trace: it says **that** the runs diverged and at which draw. The call-count tools take it from there. And it doubles the cost of every seed, so it belongs in a dedicated CI job or a debugging session. Like exploration it needs factory workloads (`workload_factory` or `workloads`), because an instance workload would carry its state into the second run.

The end-to-end tripwire for moonpool itself is `crates/moonpool-sim/tests/determinism.rs`: two full `SimulationBuilder` runs of racing workloads on the same seed must produce byte-identical traces.

## Many Seeds

One seed tests one path. Confidence needs many. The builder has two iteration modes.

**UntilCoverageStable** is the default: `plateau_seeds: 10, max_iterations: 1000`. The run keeps drawing seeds until every observed `assert_sometimes!` and `assert_reachable!` has fired and code coverage has not grown for 10 consecutive seeds, capped at 1,000. Under `cargo xtask sim run` the coverage signal is real sancov edge coverage. Otherwise it is assertion-slot coverage. Tune it with:

```rust,ignore
SimulationBuilder::new()
    .until_coverage_stable(10, 5_000)   // 10 quiet seeds, 5,000 cap
```

**FixedCount** runs a set number of seeds:

```rust,ignore
SimulationBuilder::new()
    .set_iterations(100)
```

Each seed also draws its own **swarm**: with `ChaosMode::Swarm` a seed enables a random subset of fault families, and `swarm_operations()` enables a random subset of each workload's operations. Two seeds therefore differ in which behaviours are even possible, not only in timing. [Swarm Testing](../part3-building/28-swarm-testing.md) explains why this finds more bugs than every fault on at once.

Seed-driven testing compounds. Run 1,000 seeds in CI on every commit. Run 100,000 overnight. A bug that needs three unlikely events to coincide surfaces within a few thousand seeds, because buggify amplifies the probability of each event.

## Debugging a Failing Seed

When a run fails, the report lists the seeds:

```text
  Faulty seeds: [17429853261]
```

Pin that seed and run it alone:

```rust,ignore
SimulationBuilder::new()
    .set_iterations(1)
    .set_debug_seeds(vec![17429853261])
    .trace_level(tracing::level_filters::LevelFilter::DEBUG)
    // ... the same processes and workloads ...
    .run()?;
```

The simulation replays the exact same execution. `trace_level` changes what the timeline captures and never changes scheduling or randomness, so the seed replays identically at any level. Set a breakpoint. Step through. The bug is deterministic now. [Debugging a Failing Seed](../part3-building/20-debugging.md) walks through the full procedure.
