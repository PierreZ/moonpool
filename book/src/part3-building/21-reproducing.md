# Reproducing with FixedCount

<!-- toc -->

The report said seed `17429853261` failed. The first step is always the same: run that seed alone.

## Pinning a Seed

Add two calls to the builder that failed:

```rust
SimulationBuilder::new()
    .workload_factory(|| Box::new(KvWorkload::new(200)))
    // ... the same processes, chaos and invariants as the campaign ...
    .set_iterations(1)
    .set_debug_seeds(vec![17429853261])
    .run()?
```

`set_debug_seeds` supplies the seed. `set_iterations(1)` switches to `IterationControl::FixedCount(1)` so the runner stops after it. Both are needed: with the default `UntilCoverageStable` control the runner uses the pinned seed first and then keeps drawing fresh seeds until the plateau. Everything else on the builder must stay as it was. The chaos surfaces, the process counts and the workload factories all draw from the stream, so a different builder is a different schedule and the seed means nothing.

Run it. The same `[ASSERTION FAILED]` line, the same panic, the same deadlock, every time.

## Where the Builder Lives

**A nextest test.** Most integration tests build their own `SimulationBuilder`. Add the two calls locally and run the one test:

```bash
nix develop --command cargo nextest run -p moonpool-sim leader_election --nocapture
```

Do not commit the pin. The schedule moves with the next runtime change and the pinned seed stops meaning anything.

**An example binary.** The binaries in `crates/moonpool-sim-examples/src/bin/sim/` set seeds in code, not on the command line. Edit the builder call the same way and rerun through xtask, or through `cargo run --bin sim-<name>` for a faster, coverage-blind run.

**An exploration find.** The report's exploration section carries a `BugRecipe { seed, recipe }`. Replay the exact timeline, root seed plus RNG breakpoints, with one call:

```rust
.replay_timeline(seed, recipe)
```

It sets the seed, `FixedCount(1)`, and installs the recipe's `(rng_call_count, new_seed)` breakpoints before the run. [Exemplars and Continuations](../part5-building-on-top/05-exemplars-continuations.md) explains the recipe format.

## Turning Up the Logging

A campaign runs quiet. A pinned seed can afford the firehose. Two knobs, and no `RUST_LOG` plumbing:

```rust
moonpool_sim::init_sim_tracing(tracing::Level::DEBUG);

SimulationBuilder::new()
    .trace_level(tracing::level_filters::LevelFilter::TRACE)
    // ...
```

`init_sim_tracing` sets the stdout subscriber's level. `trace_level` sets the floor the simulation layer enables and the timeline captures. At the default `INFO` every `DEBUG` and `TRACE` span in process and workload code is disabled before the registry allocates it. Lower the floor and those spans come alive and their events join the timeline, at the price of a slower run and a much larger capture. The floor never changes scheduling or randomness, so the seed replays identically at any level.

Print simulated time on every line, not wall-clock time: `SimTime` from [Events and Invariants](./17-events-and-invariants.md#simulation-time-in-log-output) does that, and it is the difference between a log you can read and one you cannot.

## When the Bug Disappears

You pinned the seed, the run is green. Something draws outside the single simulation stream, or something the seed does not control decides behaviour. Add the canary:

```rust
.check_determinism()
```

The runner now runs the seed twice. The first pass records a fingerprint after every draw on the simulation stream. The second pass replays and compares. The seed fails with the always-assertion `"determinism canary: replay matched the recorded draw sequence"` and the detail names the verdict:

- `Diverged { index, expected, actual }`: draw number `index` fingerprinted differently. The stream or the logical clock had already diverged by then.
- `ExtraDraws { recorded }`: the replay drew more often than the record.
- `Unconsumed { recorded, consumed }`: the replay drew a matching prefix and then stopped early.

The canary needs factory workloads: an instance workload carries state into the second run and diverges by construction. `crates/moonpool-sim/tests/determinism_canary.rs` shows it tripping on a process-wide counter that survives from the first run into the second.

A canary says **that** the runs diverged and at which draw, not why. Bisect toward the index:

1. Read `moonpool_sim::rng_call_count()` at suspect points and log it. The count at the moment of the divergence is the fingerprint of "where we are".
2. Install `moonpool_sim::set_rng_breakpoints(vec![(target_count, new_seed)])` before the run to reseed the stream once the count passes `target_count`. The run before the breakpoint is unchanged, so moving the target narrows the window.
3. Compare the two runs' logs at the same count. The first line that differs is the culprit.

The usual culprits:

- A `rand::thread_rng()` or a `HashMap` iteration whose order reaches a decision.
- A wall-clock read (`Instant::now()`, `SystemTime::now()`) instead of `ctx.time()`.
- A `tokio::time::sleep`, `tokio::spawn` or `tokio::select!` instead of the provider and `moonpool_sim::select!`.
- A thread-local or `static` that survives from one seed into the next.
- A detached task from a previous seed still running.
- Real I/O: files, sockets, environment variables.

Fix the leak, drop the canary or keep it in a test, and the original seed reproduces again.

## From Reproduction to Investigation

With a reliable reproduction, the question changes from "does it happen" to "why". The event trace shows the causal chain: which events fired, in what order, with what result. With a pinned seed you can also set an IDE breakpoint and step the exact path, because the executor polls everything on one thread.

You are not searching for a bug any more. You are reading a recording of one.
