# Simulation in the Browser
<!-- toc -->

The same simulation that grinds through thousands of seeds in CI also runs in
a browser tab, with no server and no install. Type a seed, watch two nodes
trade frames over a chaotic network, and share a link that reproduces the exact
run on anyone's machine. That is a good demo. It is also a proof. If the engine
runs inside a sandbox with no operating system, nothing in the simulation
secretly depends on one.

## Why a simulator compiles to wasm at all

In simulation the network and the disk are not devices. `NetworkSimulation` is
an in-memory state machine that schedules "deliver these bytes at logical time
T" on a queue. `StorageEngine` does the same for reads, writes and syncs. Time
is a counter the scheduler advances. Randomness is a seeded ChaCha8 stream.
Tasks run on the [deterministic executor](../part2-foundations/11-executor.md),
which is plain library code. Add those up and there is no syscall on the path.

The production providers are the opposite. `TokioProviders` is real TCP, real
files, OS randomness and `getrusage`, each a trip into the kernel. That single
difference is why the simulator crosses to `wasm32-unknown-unknown` and the
production backend does not.

## What keeps the build portable

Four narrow decisions, not a rewrite.

- **The explorer is a feature.** `moonpool-sim` gates `moonpool-explorer`
  (`libc`, `fork`, `mmap`) behind the default-on `exploration` feature.
  Switch it off for wasm. Assertion accounting falls back to the in-process
  table from `moonpool-assertions`, so every coverage mode still works.
- **No tokio runtime in the sim.** `SimTaskProvider` spawns onto the
  deterministic executor. The only tokio code left in the graph is the
  `macros` feature behind `select!` and `tokio-util`'s `CancellationToken` for
  shutdown. No `rt`, no `net`, no `fs`, no `time`.
- **Seeded randomness only.** `rand` and `rand_chacha` are compiled without
  default features, so nothing reaches for `getrandom` and the OS entropy a
  browser cannot answer. The sim never needed it.
- **Wall-clock shims.** `std::time::Instant::now()` and `SystemTime::now()`
  compile on wasm and then **panic the moment you call them**. The runner uses
  them only for report timings and the default base seed, through
  `runner/wall_clock.rs`, which returns a zero duration on wasm. Set a seed
  explicitly in a wasm run. `moonpool-hyper` applies the same care: its
  `HyperTimer` mints its anchor without a clock on that target.

## Does `block_on` park?

Compiling is necessary, not sufficient. A conventional runtime that runs out
of ready work **parks** the thread, and parking panics on
`wasm32-unknown-unknown`. So the real question is whether `block_on` ever
parks while driving a simulation.

It does not. The moonpool executor never parks. The simulation is
ready-driven: every wakeup fires inline as the orchestrator steps the event
queue, and no external reactor sits on a socket or a timer to wake the thread
later. If nothing is runnable, the executor treats that as a deadlock and
panics with the seed instead of parking.

## Building a wasm-able crate

Putting a simulation in a browser is a Cargo question, not a code question.
Your workload, your processes and your invariants stay as they are. The one
rule is to keep the non-portable features out:

```toml
[dependencies]
moonpool-sim = { version = "0.8", default-features = false }
futures = "0.3"
```

`default-features = false` drops `exploration` and `tokio-providers` and leaves
a simulator that targets wasm. The same crate's tests, a multi-seed chaos
binary or a CI runner depend on the same code with the features switched back
on. Cargo resolves features per build, so `cargo build -p your-wasm-crate`
never drags the explorer into the bundle.

## A worked example

The repository ships one:
[`crates/moonpool-wasm-demo`](https://github.com/PierreZ/moonpool/tree/main/crates/moonpool-wasm-demo).
It runs one seed of two nodes trading integrity-checked ping and pong frames
over raw simulated TCP, and animates the result. The server and client are
ordinary `Process` and `Workload` code with no browser awareness:

```rust
pub fn run_seed(seed: u64) -> RunResult {
    let (data, recorder) = timeline::recorder();
    let _report = SimulationBuilder::new()
        .processes(ProcessCount::Fixed(1), || Box::new(actors::PongServer))
        .workloads(WorkloadCount::Fixed(1), |_| Box::new(actors::PingClient))
        // The recorder is an Invariant: it reads the same trace timeline
        // your correctness invariants read, and turns it into the picture.
        .invariant(recorder)
        .chaos_duration(actors::CHAOS_DURATION)
        .enable_chaos([Chaos::Network(ChaosMode::Random)])
        .set_iterations(1)
        .set_debug_seeds(vec![seed])
        .run()
        .expect("simulation configuration is valid");

    timeline::finish(seed, &data)
}
```

`Chaos::Network(ChaosMode::Random)` injects seeded latency, clogs and
connection drops, so some round trips come back slow and some never come back.
The workload asserts it: `assert_sometimes!(rtt_ms >= SLOW_RTT_MS, …)` and
`assert_reachable!("chaos drops a request before any pong returns")`. A
`#[wasm_bindgen]` wrapper exports `run_seed_json` to JavaScript as
`runSeed(seed)`, and that is the whole browser surface.

It is running right here, compiled to wasm and served as a page asset. Press
**Run**, type a seed, or pick a preset. `42` is calm, `123` has some drops,
`256` is chaotic, `7` is a storm. The same seed always replays the same
history.

<iframe
  src="../wasm-demo/index.html?embed=1&amp;seed=256"
  title="moonpool ping-pong wasm demo"
  loading="lazy"
  style="width:100%;height:640px;border:1px solid #30363d;border-radius:12px;background:#0d1117;color-scheme:dark">
</iframe>

To run it outside the book:

1. Build the library for wasm:
   `cargo build --release --target wasm32-unknown-unknown -p moonpool-wasm-demo --lib`
2. Generate the JavaScript glue:
   `wasm-bindgen --target web --out-dir crates/moonpool-wasm-demo/web/pkg target/wasm32-unknown-unknown/release/moonpool_wasm_demo.wasm`
3. Serve `crates/moonpool-wasm-demo/web/` with any static file server and
   press **Run**.

`book/build-wasm-demo.sh` runs those steps and stages the result in
`book/src/wasm-demo/`, which is where the iframe above loads from. Run it
before `mdbook build`. The `wasm-bindgen` crate version must match the CLI
exactly (`0.2.129` in the Nix flake), or bindgen fails with an opaque schema
error.

Run a seed natively with `cargo run -p moonpool-wasm-demo 42`, then run the
same seed in the browser. You get the **byte-identical** history: the native
binary prints the same JSON `runSeed` returns. Same seed, same run, on your
laptop and in a stranger's tab.

## What stays behind

Two things do not cross over, by design. The production providers cannot run
in a browser, because a tab has no raw TCP and no filesystem. The explorer
cannot follow, because there is no `fork` in wasm. You lose multiverse
forking, not correctness: the in-process assertion table still tracks every
`assert_always!` and `assert_sometimes!`. The platform matrix lives in
[Using Providers in Production](./05-production.md#where-it-runs).

To keep this path from rotting, a `portability` job in CI runs on every
change. It lints and tests `moonpool-sim` with `--no-default-features`, then
runs `cargo check --target wasm32-unknown-unknown` for `moonpool-assertions`,
`moonpool-journal`, `moonpool-sim`, `moonpool-wasm-demo` and `moonpool-rpc`.
A separate job builds the demo and the book together, so a broken embed fails
the pull request. The browser is not a one-time stunt. It is a gate.
