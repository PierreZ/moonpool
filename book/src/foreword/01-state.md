# State of Moonpool

<!-- toc -->

Moonpool is a hobby project under active development. It is not production-ready. There is no backward-compatibility policy: when an API changes, the old one is deleted, not deprecated. The workspace is at version `0.8.0`. This book documents the framework as it exists today and marks what is new.

## The crates

The workspace holds sixteen crates. Ten are published to crates.io. Six are tools and examples that stay in the repository. The [Crate Map](../appendix/02-crate-map.md) has the dependency graph.

| Crate | What it is | Status |
|---|---|---|
| `moonpool` | The facade. Features `sim` and `tokio` are on by default. `hyper`, `prometheus`, `rpc`, `rpc-derive`, `rpc-tls`, `rpc-jwt` and `compio` are opt-in. A lean production build uses `default-features = false, features = ["tokio"]`. | Stable |
| `moonpool-core` | The six provider traits (`TimeProvider`, `TaskProvider`, `NetworkProvider`, `RandomProvider`, `StorageProvider`, `SystemProvider`), `LocalTaskProvider`, `LocalProviders`, `TokioProviders`, `select!`, `LayoutRegion`, storage types. Granular `tokio-*` features keep it wasm-clean. | Stable. `SystemProvider` and `LayoutRegion` are new. |
| `moonpool-sim` | The simulation engine: `SimWorld`, the deterministic executor, `SimulationBuilder`, chaos, the 15 assertion macros, events and invariants. Compiles to `wasm32-unknown-unknown` with `--no-default-features`. | Stable core. See the new features below. |
| `moonpool-buggify` | `buggify!`, `buggify_with_prob!`, `buggify_named!`, `hint!` and `cpu_charge`. Pure std, zero dependencies, inert outside a simulation. | Stable. `hint!`, `buggify_named!`, `set_activation` and `cpu_charge` are new. |
| `moonpool-assertions` | Antithesis-style assertion accounting: 2048 slots, 256 buckets, the heap table the explorer overlays with shared memory. Pure std, wasm-able. | Stable |
| `moonpool-explorer` | Fork-based frontier exploration: recipes, the frontier controller, bounded workers, sancov edge coverage. Linux and macOS only. | Stable. Multi-process parallel exploration is still on the list. |
| `moonpool-hyper` | hyper 1.x on the providers: `HyperExecutor`, `HyperTimer`, `HyperIo`, `TowerToHyperService`. Runs tonic, axum and plain hyper unchanged. | Stable |
| `moonpool-rpc` | Typed dynamic-endpoint RPC in the style of FoundationDB's `fdbrpc`: interfaces as data, delivery modes, streams, per-endpoint admission queues, buggify points inside the runtime. Features `prost`, `derive`, `tls`, `jwt`. | New |
| `moonpool-rpc-derive` | The `#[moonpool_rpc::service]` attribute. | New |
| `moonpool-prometheus` | `PrometheusSource`: the metrics your code already keeps, reported per node and across seeds. | Stable |
| `moonpool-journal` | A CLSTORE journal: positions in any order, crash told apart from corruption, damage identified for repair from peers. | New. Rewritten this cycle. |
| `moonpool-rpc-sim` | The simulation harness, oracles and six CI campaigns for `moonpool-rpc`. Not published. | New |
| `moonpool-sim-examples` | Runnable simulations found by `xtask`: maze, dungeon, axum, tonic, journal, topology, metrics. Not published. | Stable |
| `moonpool-wasm-demo` | The browser demo embedded in [Simulation in the Browser](../part4-integration/07-wasm.md). Not published. | Stable |
| `moonpool-calibrate` | Measures a real host's disk and network and prints `LatencyDistribution` constants. Not published. | Stable |
| `xtask` | `cargo xtask sim list | run | run-all`, with sancov instrumentation for the explorer. Not published. | Stable |

## What is stable

The **simulation engine** is the oldest and most exercised part. One OS thread, one seeded random stream, logical time that jumps when every task is blocked. The same seed gives the same execution, and `SimulationBuilder::check_determinism` runs every seed twice to check that claim. The engine runs the example simulations and the RPC campaigns in CI on every commit.

**Chaos** covers the network (partitions, latency, close, corruption, partial writes), storage (corruption, torn writes, misdirection, sync failures, stalls, the barrier-bounded crash model) and process attrition (graceful reboots and crashes). `ChaosMode::Swarm` enables a random subset of fault families per seed. `buggify_knob!` pushes configuration knobs to extremes.

The **assertion suite** has the 15 macros of the Antithesis taxonomy: always, sometimes, reachable, unreachable, eight numeric forms, `assert_sometimes_all!` and `assert_sometimes_each!`. The explorer forks a child at every discovery and keeps the accounting in shared memory.

**Events and invariants** are plain `tracing` events. `SimulationLayer` captures them, and invariants read them through `TraceQuery` after every step.

**Integration** runs real HTTP/2, axum and tonic traffic through `moonpool-hyper`, and the simulator runs in the browser through wasm.

## What is new

These landed in the last cycle. Each has a chapter.

- **`hint!`**: shipped code names a moment, and the simulator decides what goes wrong there. `HintVeto` lets a harness cap how many hints strike. See [hint!: Naming Fault Sites](../part3-building/29-hints.md).
- **`buggify_named!` and `set_activation`**: per-seed scenarios driven by named sites. See [Buggify](../part3-building/08-buggify.md).
- **`Chaos::Outage` and `OutageVeto`**: a correlated outage takes a whole process group down at one instant, and a harness can defer it. **`SimContext::crash_self`** and `SelfCrash` let a workload crash its own process. A `.cluster()` group may draw zero processes. See [Correlated Outages and Self-Crashes](../part3-building/30-outages.md).
- **CPU time and gray failures**: `Chaos::Cpu`, `Chaos::GrayFailure`, `cpu_charge` and `SimContext::slowness`. A machine stays alive but slow. `SystemProvider` exposes CPU and disk counters to the code under test. See [CPU Time and Gray Failures](../part3-building/26-cpu-and-gray-failure.md).
- **Aimed storage faults**: `storage_fault_mask`, owner-scoped and byte-range targeted injection, `LayoutRegion`, `FaultContext::durable_bytes`, and the replicated patterns `Minority`, `Striped` and `Rolling` per process group. See [Aiming Storage Faults](../part3-building/33-aimed-faults.md).
- **`moonpool-journal` v2**: a position-keyed CLSTORE journal with `Batch::fits` and `fits_another`, `Journal::peek_meta`, an `Ambiguous` tail and hints inside a commit. See [A Crash-Aware Journal](../part5-building-on-top/08-journal.md).
- **`moonpool-rpc`**: admission queues per endpoint, the `derive`, `tls` and `jwt` features, and the `moonpool-rpc-sim` campaigns. See [Typed RPC with moonpool-rpc](../part4-networking/02-rpc.md).

## What is missing

Multi-process parallel exploration across CPU cores is not implemented. The explorer runs one controller and a bounded pool of short-lived forked workers. There is no deterministic hypervisor: code that bypasses the providers is not simulated.

## How to read this book

Part I is motivation. You can read it without touching code, and the ideas apply to any simulation framework. Parts II through VI follow the current API. Every example is adapted from a test or an example in the repository. Part VI describes the most novel piece, fork-based exploration. If you are evaluating whether exploration matters for your system, start there.
