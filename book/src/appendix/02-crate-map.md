# Crate Map

<!-- toc -->

The workspace has 16 members at version 0.8.0. Application code depends on `moonpool` (the facade) or directly on the crate it needs.

## Dependency Graph

```text
moonpool (facade)
 |- moonpool-core                    provider traits, core types
 |- moonpool-sim (feature sim)       simulation runtime
 |   |- moonpool-core [deterministic-select]
 |   |- moonpool-assertions
 |   |- moonpool-buggify
 |   `- moonpool-explorer (feature exploration)
 |        `- moonpool-assertions
 |- moonpool-hyper (feature hyper)        -> moonpool-core
 |- moonpool-prometheus (feature prometheus) -> moonpool-core
 `- moonpool-rpc (feature rpc)            -> moonpool-core, moonpool-buggify
      `- moonpool-rpc-derive (feature rpc-derive)

moonpool-journal   -> moonpool-core, moonpool-buggify   (not in the facade)
moonpool-rpc-sim   -> moonpool-rpc, moonpool-sim, moonpool-core (unpublished campaigns)
moonpool-sim-examples -> moonpool-sim, moonpool-hyper, moonpool-prometheus, moonpool-journal
moonpool-wasm-demo -> moonpool-sim (no default features)
moonpool-calibrate -> no moonpool crate (standalone CLI)
xtask              -> no moonpool crate
```

Production builds keep the simulation out: `moonpool = { default-features = false, features = ["tokio"] }`. `moonpool-sim` is never a dependency of `moonpool-rpc`, `moonpool-hyper`, `moonpool-journal` or `moonpool-prometheus` (only a dev-dependency).

## Library Crates

| Crate | Role | Key public items | Features |
|-------|------|------------------|----------|
| `moonpool` | Facade. Re-exports `moonpool_core::*` and, with `sim`, `moonpool_sim::*`. Modules `hyper`, `prometheus`, `rpc` behind features. `prelude` with `Process`, `SimContext`, `SimulationBuilder`, `Workload`. | everything below | `default = ["sim", "tokio"]`, `sim`, `tokio`, `hyper`, `prometheus`, `rpc`, `rpc-derive`, `rpc-tls`, `rpc-jwt` |
| `moonpool-core` | Provider traits and core types. wasm-clean with all features off. | `TimeProvider`, `TaskProvider`, `NetworkProvider`, `TcpListenerTrait`, `RandomProvider`, `StorageProvider`, `StorageFile`, `SystemProvider`, `Providers`, `Resolver`, `Detach`, `JoinError`, `OpenOptions`, `DirectIo`, `IoConstraints`, `AlignedBuf`, `BlockFile`, `LayoutRegion`, `SystemSample`, `DiskCounters`, `SimulationError`, `SimulationResult`, `MetricsSource`, `MetricSample`, `MetricValue`, `select!`, `Tokio*` providers | `default = ["tokio-providers", "select"]`; `tokio-providers` = `tokio-task` + `tokio-time` + `tokio-net` + `tokio-fs` + `tokio-random` + `tokio-system`; `select`; `deterministic-select` |
| `moonpool-sim` | Simulation runtime: executor, `SimWorld`, network and storage simulation, chaos, runner, assertions wiring, observability. | `SimulationBuilder`, `SimContext`, `Process`, `Workload`, `FaultInjector`, `FaultContext`, `Chaos`, `ChaosMode`, `IterationControl`, `SimulationReport`, `SimWorld`, `SimProviders`, `Invariant`, `TraceQuery`, assertion macros, `buggify_knob!`, `ExplorationConfig` | `default = ["tokio-providers", "exploration"]` |
| `moonpool-assertions` | Assertion accounting. Pure std, zero dependencies, wasm-able. Heap table by default, `MAP_SHARED` overlay under exploration. | `AssertKind`, `AssertCmp`, `DiscoveryKind`, `DiscoveryHooks`, `reachable!`, `sometimes!`, `MAX_ASSERTION_SLOTS`, `MAX_EACH_BUCKETS`, `MAX_EACH_KEYS` | none |
| `moonpool-buggify` | Fault-injection macros and state. Pure std, zero dependencies, wasm-able. Inert until a random source is installed. | `buggify!`, `buggify_with_prob!`, `buggify_fault_with_prob!`, `buggify_named!`, `buggify_pick!`, `buggify_range!`, `hint!`, `cpu_charge`, `set_activation`, `buggify_init`, `buggify_reset`, `is_simulated` | none |
| `moonpool-explorer` | Frontier exploration controller: recipes, bounded worker pool, shared memory, sancov. `libc` only. Never wasm. | `Explorer`, `ExplorationConfig`, `ExploreJob`, `Recipe`, `DiscoveryEvent`, `format_timeline`, `parse_timeline`, `set_rng_count_hook`, `sancov_is_available`, `sancov_edges_covered`, `explorer_is_child` | none |
| `moonpool-hyper` | hyper 1.x over the providers. Generic over `P: Providers`. | `HyperIo`, `HyperExecutor`, `HyperTimer`, `TowerToHyperService`, `ReconnectingChannel`, `H2Channel`, `ChannelConfig`, `KeepAlive`, `H2Server`, `H2ServerConfig`, `DEFAULT_DRAIN_TIMEOUT` | `default = ["client", "server"]` |
| `moonpool-rpc` | Typed dynamic-endpoint RPC over the providers. | `RpcDriver`, `RpcHandle`, `ServiceClient`, `RequestStream`, `IncomingRequest`, `ReplyHandle`, `ServiceRef`, `InterfaceRef`, `ServiceGroup`, `RpcConfig`, `EndpointQueue`, `ResourceLimits`, `FailureMonitor`, `RpcError`, `ErrorReason`, `Wire`, `CodecId`, `Connector`, `Acceptor`, `Plaintext`, `#[moonpool_rpc::service]` | `default = ["prost"]`, `prost`, `derive`, `tls`, `jwt` |
| `moonpool-rpc-derive` | Proc macro behind `moonpool-rpc/derive`. | `service` attribute | none |
| `moonpool-prometheus` | Prometheus registry adapter: one `PrometheusSource` per node, scraped into the report. | `PrometheusSource`, `SimCounter`, `SimCounterVec`, `SimGauge`, `SimGaugeVec`, `SimHistogram`, `SimHistogramVec`, `SimTimer` | none |
| `moonpool-journal` | CLSTORE write-ahead journal over `StorageProvider`. Positions in any order, crash told apart from corruption, identified damage, three `hint!` moments per batched commit, durable writes (`pwritev2(RWF_DSYNC)`) on the ordered path, spare segments prepared in the background. | `Journal`, `Batch`, `JournalConfig`, `JournalId`, `Durability`, `Geometry`, `Id`, `ID_SIZE`, `State`, `Recovery`, `Rolls`, `Layout`, `Entry`, `CommitError`, `OpenError`, `ReadError`, `Durable`, `BLOCK`, `RECORD_SIZE`, `META_MAX`, `MAX_BATCH_RECORDS` | none |

## Binary and Harness Crates

| Crate | Role | Binaries |
|-------|------|----------|
| `moonpool-sim-examples` | Runnable simulation workloads that xtask discovers. | `sim-maze-explore`, `sim-dungeon-explore`, `sim-axum-web`, `sim-metrics-service`, `sim-topology`, `sim-tonic-grpc`, `sim-journal` |
| `moonpool-explorer` | Also ships the explorer's own ladder scenario. | `sim-frontier-explore` |
| `moonpool-rpc-sim` | Simulation harness, oracles and campaigns for `moonpool-rpc`. Unpublished. | `sim-rpc-foundations`, `sim-rpc-delivery`, `sim-rpc-interfaces`, `sim-rpc-balance`, `sim-rpc-streams`, `sim-rpc-security`, `sim-rpc-qualification` |
| `moonpool-calibrate` | Measures real storage and network latency on the host and prints `LatencyDistribution` constants as Rust. Depends on no moonpool crate. | `moonpool-calibrate` |
| `moonpool-wasm-demo` | Browser demo embedded in the book. `run_seed(seed)`, `run_seed_json(seed)`. | wasm library |
| `xtask` | Cargo automation: the simulation binary runner. See [Running Simulation Binaries with xtask](./08-xtask.md). | `cargo xtask` |

The `moonpool` facade also carries examples: `retrying_worker` and `metastable_grpc_retry_storm`. `moonpool-rpc` carries `recruitment`, `balanced_mutation` and `stream_saturation`.

## Choosing a Dependency

| You want | Depend on |
|----------|-----------|
| Write a service that runs in production and in simulation | `moonpool` with `default-features = false, features = ["tokio"]` in the library, `moonpool` (defaults) in dev-dependencies |
| Write a library with no simulation dependency, but with probes | `moonpool-core` (no default features), `moonpool-assertions`, `moonpool-buggify` |
| Run simulations from a test or binary | `moonpool-sim` or `moonpool` with `sim` |
| HTTP/2, gRPC through tonic | `moonpool-hyper` or `moonpool` with `hyper` |
| Typed RPC between processes | `moonpool-rpc` or `moonpool` with `rpc` (`rpc-derive` for the attribute) |
| Report your Prometheus metrics in the simulation report | `moonpool-prometheus` or `moonpool` with `prometheus` |
| A crash-aware log for a consensus node | `moonpool-journal` |
| Compile to `wasm32-unknown-unknown` | `moonpool-sim` with `default-features = false` |

## Validation Commands

| Command | Checks |
|---------|--------|
| `nix develop --command cargo fmt` | formatting |
| `nix develop --command cargo clippy` | pedantic lints, all crates |
| `nix develop --command cargo nextest run` | all tests |
| `nix develop --command cargo clippy -p moonpool-sim --no-default-features --all-targets -- -D warnings` | the feature-less sim |
| `nix develop --command cargo check --target wasm32-unknown-unknown -p moonpool-sim --no-default-features` | the wasm build |
| `nix develop --command cargo xtask sim run-all` | every simulation binary with sancov |
| `nix develop --command mdbook build book/` | this book |
