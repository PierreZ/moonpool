# Moonpool

Deterministic simulation testing for distributed systems in Rust.

We write the system once against provider traits. In tests, the simulator
runs it on logical time, one seeded random stream and a simulated network
and disk, and injects faults. In production, the same code runs on Tokio, or on compio for thread-per-core.
One seed gives one exact execution, so every failure replays bit for bit.

Moonpool follows [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html),
with assertion and exploration ideas from [Antithesis](https://antithesis.com/).

> **Note:** This is a hobby-grade project under active development.

## Same code in production and simulation

Shipped code is never wrapped in a simulation variant. There is no `SimDisk`,
no `SimMachine` and no test-only hook in the product.

- Code depends on provider traits, usually through one `Providers` bundle:
  `TokioProviders` (or `CompioProviders`) in production, `SimProviders` in tests.
- Code names interesting moments with `hint!("label").await`. In production
  it is a no-op. In simulation the simulator decides whether to strike there,
  for example by rebooting the process.
- Code forces its own rare branches with `buggify!`, `buggify_with_prob!`
  and `buggify_named!`. They are inert outside a simulation.

## Quick start

A server process accepts connections. A client workload connects to it
under swarm network chaos. Ten seeds run in milliseconds of wall time. The
same code is the crate-level doc test of `moonpool`, so it compiles against
the current API.

```toml
[dev-dependencies]
moonpool = "0.8"
async-trait = "0.1"
```

```rust
use std::time::Duration;

use async_trait::async_trait;
use moonpool::prelude::*;
use moonpool::{Chaos, ChaosMode, SimulationResult, assert_always, assert_sometimes};

/// The system under test. A fresh instance boots on every (re)start.
struct Server;

#[async_trait]
impl Process for Server {
    fn name(&self) -> &str {
        "server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            // The deterministic select!: the branch order comes from the seed.
            moonpool::select! {
                accepted = listener.accept() => drop(accepted),
                () = ctx.shutdown().cancelled() => return Ok(()),
            }
        }
    }
}

/// The test driver. It survives server reboots and judges the run.
struct Client;

#[async_trait]
impl Workload for Client {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let servers = ctx.topology().all_process_ips().to_vec();
        assert_always!(servers.len() == 1, "one server process per seed");
        for _ in 0..20 {
            // Bound every step with simulated time, so no fault wedges the client.
            let attempt = ctx
                .time()
                .timeout(Duration::from_millis(500), ctx.network().connect(&servers[0]))
                .await;
            assert_sometimes!(matches!(attempt, Ok(Ok(_))), "client connects");
            let _ = ctx.time().sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
}

#[test]
fn server_accepts_under_chaos() -> Result<(), moonpool::SimulationError> {
    let report = SimulationBuilder::new()
        .processes(1, || Box::new(Server))
        .workload(Client)
        .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
        .set_iterations(10)
        .run()?;
    assert_eq!(report.failed_runs, 0);
    Ok(())
}
```

[`crates/moonpool/examples/retrying_worker.rs`](crates/moonpool/examples/retrying_worker.rs)
shows the other half: one function that runs on `TokioProviders` in `main`
and under the simulator in its own `#[test]`.

## Feature tour

### Providers (`moonpool-core`)

- Six provider traits: `TimeProvider`, `TaskProvider`, `NetworkProvider`,
  `RandomProvider`, `StorageProvider` and `SystemProvider`, bundled by
  `Providers`.
- `TokioProviders` for production, with granular `tokio-*` features.
  `SimProviders` for simulation.
- `LocalTaskProvider` and `LocalProviders` for `!Send` tasks (`Rc`,
  `RefCell`), plus `TimeProvider::timeout_local`. The simulator
  and `CompioProviders` implement them. The
  work-stealing `TokioProviders` does not.
- `CompioProviders` (`moonpool-compio`, feature `compio`): thread-per-core
  production on io_uring with a polling fallback.
- `Resolver` for `host:port` lookups, outside the bundle: `TokioResolver` in
  production, `ScriptedResolver` in simulation.
- `SystemProvider` reads cumulative CPU and disk counters (`getrusage`,
  `/proc/stat`, `/proc/diskstats`). The simulator keeps deterministic disk
  counters per process.
- `select!`: tokio's macro in production, and a seeded branch offset in
  simulation (`moonpool_sim::select!`).

### Simulation runner (`moonpool-sim`)

- Deterministic single-threaded executor. Task order, `select!` offsets and
  every fault coin are draws on one seeded stream.
- `Process` (the system under test, rebooted with fresh memory) and
  `Workload` (the test driver, with `setup`, `run` and `check`).
- `SimulationBuilder`: `processes` with fixed or random counts, `cluster`
  with datacenter, zone and machine locality, `tags`, `workload`,
  `workload_factory` and `workloads`.
- `ctx.deployment()`: a workload changes the deployment mid-run. `spawn`,
  `spawn_in` and `spawn_group` boot new processes, `retire` stops one for
  good, and topology queries and chaos victim pools follow the live
  membership. An operator is an ordinary workload (`max_spawns` caps it).
- Iteration control: `set_iterations`, `until_coverage_stable`,
  `set_debug_seeds`, `run_time_budget`, `replay_timeline`.
- `check_determinism` runs each seed twice and fails it when the draw
  sequences differ.
- `SimulationReport` with failing seeds, assertion and coverage violations,
  exploration results, application metrics and metric queries.

### Chaos and faults

- `enable_chaos` takes `Chaos` surfaces, each sampled per seed by
  `ChaosMode::Random` or `ChaosMode::Swarm`.
- `Chaos::Network`: latency, clogs, partitions, black holes, bit flips,
  random closes, connect failures, clock drift and per-pair latency.
  `link_latency` sets latency by locality.
- `Chaos::Storage`: corruption, EIO, misdirected and phantom writes, sync
  failures, short transfers, lost directory entries, crash damage,
  degradation and disk failure. The crash model is bounded by sync barriers,
  with an oracle for lost synced writes.
- `Chaos::Attrition`: graceful, crash and wipe reboots under a `max_dead`
  budget, scoped per process, machine, zone or datacenter. An optional wipe
  budget makes wipes roll: a wiped process stays unhealed until the harness
  signals it healed, and a wipe past the budget becomes a crash.
- `Chaos::Outage`: a correlated outage of whole process groups, with an
  optional straggler. An `OutageVeto` lets the harness defer it.
- `Chaos::Cpu` and `Chaos::GrayFailure`: a CPU model per process and slow
  but alive machines. Workloads read `SimContext::slowness`.
- `Chaos::BuggifyKnobs` moves network and storage knobs to extremes.
- `SimContext::crash_self` and `SelfCrash`: a process crashes itself, as a
  power loss would.
- Aimed storage faults: owner-scoped, byte-range targeting through
  `LayoutRegion` and `FaultFocus`, and `replicated_storage_faults` with
  minority, striped and rolling patterns that never hit every replica.
- Full disks: `set_disk_capacity` gives a disk a size (optionally
  amplified), so growth past it fails with `ENOSPC` and a delete or a
  truncate frees space. `SystemSample::space` reports the free bytes
  (`statvfs` in production).
- Custom faults: `fault_factory` with a `FaultInjector` and its
  `FaultContext`. `chaos_duration` closes the chaos window and opens a
  quiet recovery tail; `chaos_until_run_ends` keeps chaos on for the whole
  run phase and runs `check()` on the live processes after it.

### `hint!`, buggify and swarm testing (`moonpool-buggify`)

- `hint!("label").await` names a moment. The simulator may reboot the
  caller there, under the seed's attrition regime and an optional `HintVeto`,
  which also guards a per-process regime's timed reboots.
- `buggify!`, `buggify_with_prob!`, `buggify_fault_with_prob!`,
  `buggify_pick!` and `buggify_range!`: two-phase activation, once per run
  and then per call. `buggify_knob!` (in `moonpool-sim`) for tunables.
- `buggify_named!` with `set_activation` for per-seed scenarios.
- `cpu_charge` reports CPU work to the CPU model, inert elsewhere.
- Swarm testing: each seed enables a random subset of fault families and
  workload operations (`ChaosMode::Swarm`, `network_fault_mask`,
  `storage_fault_mask`, `swarm_operations` with `swarm_op_enabled`). We
  prefer one workload with swarm testing over many separate workloads.

### Assertions and invariants (`moonpool-assertions`)

- Fifteen Antithesis-style macros. Boolean: `assert_always!`,
  `assert_always_or_unreachable!`, `assert_sometimes!`, `assert_reachable!`,
  `assert_unreachable!`. Numeric: four `assert_always_*` and four
  `assert_sometimes_*` comparisons. Compound: `assert_sometimes_all!` and
  `assert_sometimes_each!`.
- They record and never panic. Always-failures are bugs. Sometimes-sites
  that never fire are coverage gaps.
- `reachable!` and `sometimes!` probes for shipped code.
- Events and invariants through plain `tracing`: `SimulationLayer` captures
  the timeline, `TraceQuery` reads it, and `invariant` / `invariant_fn` check
  it after every simulation step. Simulator faults appear in the same
  timeline as `SimFaultEvent`s.

### Exploration (`moonpool-explorer`)

- `enable_exploration` with `ExplorationConfig`: when an assertion
  discovers something new, the explorer keeps a replayable recipe and
  schedules bounded continuations from it.
- A fixed pool of forked workers (`workers: 0` runs in process), a frontier
  queue, exemplars per semantic state, and `BugRecipe` replay.
- Optional code-coverage guidance through sancov instrumentation, enabled
  by `cargo xtask`.

### Networking, RPC and hyper

- Raw TCP over `NetworkProvider`: test your own protocol byte for byte.
- `moonpool-rpc`: typed dynamic endpoints, delivery modes, reply streams,
  balancing, per-endpoint admission queues, buggify points inside the
  runtime, `#[service]` derive, TLS (mutual TLS included, simulated by
  `sim-rpc-tls`) and JWT features.
  `moonpool-rpc-sim` holds its simulation campaigns.
- `moonpool-hyper`: hyper 1.x, tonic gRPC and axum on the providers, with a
  reconnecting h2 channel and a per-connection server.

### Storage and the journal

- `StorageProvider` and `StorageFile`: positioned I/O, `sync_all` and
  `sync_data`, a durable positioned write (`write_at_durable`, one
  `pwritev2(RWF_DSYNC)` on Linux), `sync_dir`, and `OpenOptions::direct_io` with `IoConstraints`
  and `AlignedBuf`. Zero-copy positioned I/O: the `*_aligned` methods move an
  owned `AlignedBuf` to the kernel and back, where the borrowed form copies
  through a bounce buffer. `BlockFile` adds block addressing.
- `moonpool-journal`: a position-keyed CLSTORE journal that tells a crash
  from corruption and names the damaged entry. Positions in any order,
  `Batch::fits` / `fits_another`, `Journal::peek_meta`, metadata repair,
  ambiguous tails, layout regions for aimed faults, `hint!` points inside
  each commit, and spare segments prepared in the background
  (`Journal::prepare_spares_with`) so a segment roll is a header write and a
  rename instead of a zero-fill on the commit path. Batches, headers and
  metainfo are encoded in place in aligned buffers and written with no copy.

### Metrics (`moonpool-prometheus`)

- `metrics_factory` gives each node a `MetricsSource`. `PrometheusSource`
  records every change on the simulated clock.
- `metric` with `MetricQuery` asks questions of the series: rate,
  bucketize, fill, map, reduce and percentiles.

### WebAssembly

- `moonpool-sim` with `--no-default-features` compiles for
  `wasm32-unknown-unknown`. `moonpool-wasm-demo` runs a seeded ping-pong
  simulation in the browser.

### Tooling

- `cargo xtask sim list | run <filter> | run-all` runs the simulation
  binaries (`moonpool-sim-examples`, explorer scenarios, RPC campaigns).
- `moonpool-calibrate` measures a real disk and network and emits
  `LatencyDistribution` constants for the simulator.

## Crates

| Crate | Purpose | Published |
|-------|---------|-----------|
| `moonpool` | Facade: features `sim`, `tokio`, `compio`, `hyper`, `prometheus`, `rpc`, `rpc-derive`, `rpc-tls`, `rpc-tls-pki`, `rpc-jwt` | yes |
| `moonpool-core` | Provider traits, `TokioProviders`, local spawn traits, storage and metrics types | yes |
| `moonpool-compio` | Providers on compio for thread-per-core deployment | yes |
| `moonpool-sim` | Simulator, runner, chaos, assertion macros | yes |
| `moonpool-buggify` | `buggify!`, `hint!`, `cpu_charge`; zero dependencies | yes |
| `moonpool-assertions` | Assertion accounting; zero dependencies, wasm-clean | yes |
| `moonpool-explorer` | Frontier exploration with forked workers (Linux, macOS) | yes |
| `moonpool-hyper` | hyper 1.x, tonic and axum over the providers | yes |
| `moonpool-prometheus` | Prometheus registry in the simulation report | yes |
| `moonpool-rpc` | Typed dynamic-endpoint RPC | yes |
| `moonpool-rpc-derive` | `#[service]` interface generation | yes |
| `moonpool-journal` | CLSTORE journal: crash vs. corruption | yes |
| `moonpool-rpc-sim` | RPC simulation campaigns and oracles | no |
| `moonpool-sim-examples` | Example simulations (maze, dungeon, axum, tonic, journal) | no |
| `moonpool-wasm-demo` | Browser demo | no |
| `moonpool-calibrate` | Latency calibration tool | no |
| `xtask` | Simulation binary runner | no |

## Using in production

Keep the simulator and the explorer out of the release binary:

```toml
[dependencies]
moonpool = { version = "0.8", default-features = false, features = ["tokio"] }
```

This pulls the provider traits and `TokioProviders` with real TCP and
filesystem implementations, without `moonpool-sim`, `moonpool-explorer` or
the fork machinery. Add `hyper`, `prometheus` or `rpc` when the application
uses them, or `compio` for a thread-per-core deployment on `CompioProviders`. A crate that only marks fault sites can depend on
`moonpool-buggify` alone.

## Development

```bash
nix develop                          # pinned toolchain and tools
nix develop --command cargo nextest run
nix develop --command cargo xtask sim run-all
```

## Documentation

- [**The Sim Book**](https://pierrez.github.io/moonpool/): the user guide,
  from philosophy to fault reference.
- API documentation on docs.rs: [`moonpool`](https://docs.rs/moonpool),
  [`moonpool-core`](https://docs.rs/moonpool-core),
  [`moonpool-sim`](https://docs.rs/moonpool-sim),
  [`moonpool-buggify`](https://docs.rs/moonpool-buggify),
  [`moonpool-assertions`](https://docs.rs/moonpool-assertions).
- [Repository](https://github.com/PierreZ/moonpool)

## License

Apache 2.0
