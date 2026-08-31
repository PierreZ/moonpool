# Crate Map

<!-- toc -->

Moonpool is organized as a layered workspace. Core defines runtime-neutral
provider contracts. Simulation and hyper integrations build on those contracts
without depending on each other. The facade gathers the pieces applications
usually need.

## Dependency Diagram

```text
                              moonpool
                           (facade crate)
                         /       |       \
                        v        v        v
             moonpool-core  moonpool-sim  moonpool-hyper  moonpool-prometheus
                    ^          /     \          |
                    |         v       v         |
                    |  moonpool-   moonpool-     |
                    |  assertions  explorer      |
                    |                 |          |
                    |                libc        |
                    +----------------------------+

  moonpool-sim-examples  (raw TCP, axum, tonic, topology)
  moonpool-wasm-demo      (browser simulation over raw TCP)
  moonpool-calibrate      (measures the real host; depends on no moonpool crate)
  xtask                   (simulation command runner)
```

## Library Crates

### moonpool

**Role**: Facade crate. It re-exports core provider traits and, behind features,
the simulation runtime and the namespaced hyper integration.

Use the default feature set for simulation work. A production application can
select only `tokio`, then add `hyper` if it speaks HTTP or gRPC.

### moonpool-core

**Role**: The boundary between application logic and its runtime.

**Provider traits**:

- `TimeProvider` for sleeps, timeouts, and monotonic time
- `TaskProvider` for spawned futures
- `NetworkProvider` for TCP streams and listeners
- `RandomProvider` for runtime-controlled randomness
- `StorageProvider` for file operations
- `Providers` for carrying the five implementations as one bundle

The Tokio implementations provide real production I/O. Simulation supplies
deterministic implementations of the same traits.

It also holds the registry-agnostic metrics vocabulary (`MetricsSource`,
`MetricSample`, `SeriesRecorder`) and, in `metrics::query`, the typed
SELECT / RATE / BUCKETIZE / MAP / REDUCE model a runner uses to declare what it
wants summarized. Both are pure std and wasm-clean. See
[Application Metrics](../part3-building/25-metrics.md).

### moonpool-assertions

**Role**: Antithesis-style assertion accounting with no dependencies.

The default table lives on the heap. Explorer workers can overlay a shared
region so discoveries and watermarks survive process boundaries. The crate is
also usable without the simulation runner and compiles to wasm.

### moonpool-sim

**Role**: Deterministic execution, simulated time/network/storage, chaos,
process lifecycle, workloads, tracing invariants, and assertion wiring.

**Key types**:

- `SimulationBuilder` configures processes, workloads, chaos, and iterations
- `SimContext` exposes providers, topology, shared state, and shutdown
- `SimWorld` coordinates lifecycle and the global `Scheduler<Event>`
- `Scheduler<Event>` owns monotonic logical time, same-time FIFO ordering, and cancellation
- `NetworkSimulation` owns network state, topology, faults, operations, results, and wakers
- `StorageEngine` owns persistent files, independent handles, disk behavior, operations, results, and wakers
- `Process` describes the system under test
- `Workload` describes the test driver
- `Invariant` checks cross-process properties from trace events
- `NetworkConfiguration` and `StorageConfiguration` tune fault surfaces

The optional default-on `exploration` feature connects the runner to
moonpool-explorer. Disable it for `wasm32-unknown-unknown`.

### moonpool-hyper

**Role**: Run real hyper, axum, and tonic stacks over provider-backed streams.

**Key types**:

- `HyperIo` adapts a futures-io stream to hyper's I/O traits
- `HyperExecutor` routes hyper tasks through `TaskProvider`
- `HyperTimer` answers hyper clock requests through `TimeProvider`
- `TowerToHyperService` bridges tower and hyper services
- `ReconnectingChannel` provides a lazy reconnecting h2 client channel
- `H2Server` serves h2 connections with provider-driven shutdown and timing

Client and server features are individually selectable. The featureless crate
contains only the runtime adapters.

### moonpool-prometheus

**Role**: Report the metrics your application already keeps as simulation
output.

**Key types**:

- `PrometheusSource` wraps a `prometheus::Registry` and implements
  `MetricsSource`
- `SimCounter`, `SimGauge`, `SimHistogram` are instrumented handles that record
  every mutation on the simulated clock
- `SimTimer` times a histogram observation from `TimeProvider`, not the wall
  clock

It depends only on moonpool-core, never on moonpool-sim, so the adapter is
equally usable in production and moonpool-sim stays wasm-clean. See
[Application Metrics](../part3-building/25-metrics.md).

### moonpool-explorer

**Role**: Frontier exploration through replay recipes, discovery journals,
exemplars, and a bounded worker pool.

The explorer depends on moonpool-assertions for the shared discovery contract
and on `libc` for `fork`, `waitpid`, and shared mappings. It remains optional so
the simulation runtime can build on wasm.

## Workspace Applications

### moonpool-sim-examples

Runnable examples cover raw TCP topology, axum over HTTP/1, tonic over HTTP/2,
and exploration workloads. They are demonstration binaries, not library
dependencies.

### moonpool-wasm-demo

A single-seed raw TCP ping/pong simulation compiled to wasm. It exports a JSON
timeline consumed by the browser animation embedded in the book.

### moonpool-calibrate

A calibration CLI that measures the real host and prints
`LatencyDistribution` constants for moonpool's storage and network latency
knobs. Uniquely in this workspace it depends on **no moonpool crate at
runtime**: the measurement path is raw `std::fs`, `std::net`, and
`std::time::Instant`, because measuring through the providers would measure the
simulator rather than the machine. See
[Calibrating Against a Real Machine](./07-calibration.md).

### xtask

Cargo automation for discovering and running simulation binaries:

- `cargo xtask sim list`
- `cargo xtask sim run <filter>`
- `cargo xtask sim run-all`
