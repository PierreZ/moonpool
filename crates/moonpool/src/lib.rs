//! # Moonpool
//!
//! Deterministic simulation testing for distributed systems in Rust.
//!
//! We write the system once against a small set of provider traits. In
//! tests, the simulator runs it on logical time, a seeded random stream and
//! a simulated network and disk, and injects faults. In production, the
//! same code runs on [`TokioProviders`]. One seed gives one exact
//! execution, so a failing seed replays bit for bit.
//!
//! Moonpool follows [FoundationDB's simulation
//! testing](https://apple.github.io/foundationdb/testing.html), with
//! assertion and exploration ideas from [Antithesis](https://antithesis.com/).
//!
//! This crate is the facade. It re-exports [`moonpool_core`] at the root and,
//! with the `sim` feature, [`moonpool_sim`] too. Optional integrations live in
//! namespaced modules: `hyper`, `prometheus` and `rpc`.
//!
//! ## Same code in production and simulation
//!
//! Shipped code is never wrapped in a simulation variant. There is no
//! `SimDisk` and no test-only hook in the product. Code depends on the
//! provider traits ([`TimeProvider`], [`TaskProvider`], [`NetworkProvider`],
//! [`RandomProvider`], [`StorageProvider`], [`SystemProvider`]), usually
//! through one [`Providers`] bundle. Shipped code names interesting moments
//! with `moonpool_buggify::hint!("label").await` and forces rare branches
//! with `buggify!`. Both are inert outside a simulation.
//!
//! ## Quick start
//!
//! A server process accepts connections until shutdown. A client workload
//! connects to it under swarm network chaos. Ten seeds run in milliseconds of
//! wall time.
//!
//! ```rust
//! use std::time::Duration;
//!
//! use async_trait::async_trait;
//! use moonpool::prelude::*;
//! use moonpool::{Chaos, ChaosMode, SimulationResult, assert_always, assert_sometimes};
//!
//! /// The system under test. A fresh instance boots on every (re)start.
//! struct Server;
//!
//! #[async_trait]
//! impl Process for Server {
//!     fn name(&self) -> &str {
//!         "server"
//!     }
//!
//!     async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
//!         let listener = ctx.network().bind(ctx.my_ip()).await?;
//!         loop {
//!             // The deterministic select!: the branch order comes from the seed.
//!             moonpool::select! {
//!                 accepted = listener.accept() => drop(accepted),
//!                 () = ctx.shutdown().cancelled() => return Ok(()),
//!             }
//!         }
//!     }
//! }
//!
//! /// The test driver. It survives server reboots and judges the run.
//! struct Client;
//!
//! #[async_trait]
//! impl Workload for Client {
//!     fn name(&self) -> &str {
//!         "client"
//!     }
//!
//!     async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
//!         let servers = ctx.topology().all_process_ips().to_vec();
//!         assert_always!(servers.len() == 1, "one server process per seed");
//!         for _ in 0..20 {
//!             // Bound every step with simulated time, so no fault wedges the client.
//!             let attempt = ctx
//!                 .time()
//!                 .timeout(Duration::from_millis(500), ctx.network().connect(&servers[0]))
//!                 .await;
//!             assert_sometimes!(matches!(attempt, Ok(Ok(_))), "client connects");
//!             let _ = ctx.time().sleep(Duration::from_millis(50)).await;
//!         }
//!         Ok(())
//!     }
//! }
//!
//! let report = SimulationBuilder::new()
//!     .processes(1, || Box::new(Server))
//!     .workload(Client)
//!     .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
//!     .set_iterations(10)
//!     .run()?;
//! assert_eq!(report.failed_runs, 0);
//! # Ok::<(), moonpool::SimulationError>(())
//! ```
//!
//! ## What the `sim` feature adds
//!
//! - **Runner**: [`SimulationBuilder`] with process groups, tags, workloads,
//!   invariants, iteration control and a [`SimulationReport`].
//! - **Chaos**: [`Chaos`] surfaces (network, storage, attrition, outage, CPU,
//!   gray failure, buggified knobs), each sampled per seed by [`ChaosMode`].
//! - **Assertions**: fifteen Antithesis-style macros, from `assert_always!`
//!   to `assert_sometimes_each!`. They record, they never panic.
//! - **Exploration**: frontier-based multiverse exploration through
//!   `moonpool-explorer` (native targets).
//!
//! ## Cargo features
//!
//! | Feature | Default | Adds |
//! |---------|---------|------|
//! | `sim` | yes | `moonpool-sim`: the simulator, chaos, assertions, exploration |
//! | `tokio` | yes | [`TokioProviders`] and the `select!` passthrough |
//! | `hyper` | no | `moonpool::hyper`: HTTP/2, tonic and axum over the providers |
//! | `prometheus` | no | `moonpool::prometheus`: a `prometheus` registry in the report |
//! | `rpc` | no | `moonpool::rpc`: typed dynamic-endpoint RPC |
//! | `rpc-derive` | no | the `#[moonpool_rpc::service]` attribute |
//! | `rpc-tls` | no | TLS sessions for `rpc` (native only) |
//! | `rpc-jwt` | no | JWT/JWKS request verification for `rpc` (native only) |
//!
//! A production binary keeps the simulator out of its dependency tree:
//!
//! ```toml
//! [dependencies]
//! moonpool = { version = "0.8", default-features = false, features = ["tokio"] }
//! ```
//!
//! ## Which crate to use
//!
//! | Use case | Crate |
//! |----------|-------|
//! | Full framework (recommended) | `moonpool` |
//! | Provider traits and production providers only | `moonpool-core` |
//! | Simulation runtime | `moonpool-sim` |
//! | `buggify!`, `hint!` and `cpu_charge` in shipped code, zero dependencies | `moonpool-buggify` |
//! | Assertion accounting only, zero dependencies | `moonpool-assertions` |
//! | An HTTP/2 stack (tonic, axum, hyper) on the providers | `moonpool` with feature `hyper`, or `moonpool-hyper` |
//! | Prometheus metrics in the simulation report | `moonpool` with feature `prometheus`, or `moonpool-prometheus` |
//! | Typed RPC between processes (FDB-style endpoints) | `moonpool` with feature `rpc`, or `moonpool-rpc` |
//! | A journal that tells a crash from corruption | `moonpool-journal` |
//! | Fork-based exploration internals | `moonpool-explorer` |
//!
//! ## Documentation
//!
//! - [The Sim Book](https://pierrez.github.io/moonpool/): the user guide.
//! - [`moonpool_core`]: provider traits and core types.
//! - [`moonpool_sim`]: simulation runtime, chaos and assertions.

#![deny(missing_docs)]

// Re-export all public items from sub-crates. `moonpool-core` is always present;
// `sim` is feature-gated so a lean production build pulls neither the simulation
// runtime nor the explorer (no libc/mio in the prod dependency tree).
#[allow(ambiguous_glob_reexports)]
pub use moonpool_core::*;
#[cfg(feature = "sim")]
#[allow(ambiguous_glob_reexports)]
pub use moonpool_sim::*;

/// hyper 1.x integration, from [`moonpool_hyper`]. Requires the `hyper`
/// feature.
///
/// The module holds the runtime hooks hyper needs over the providers:
/// `HyperExecutor` (task provider), `HyperTimer` (time provider) and
/// `HyperIo` (a provider stream as hyper IO). On top of them it adds a
/// reconnecting h2 client channel in the shape tower and tonic expect
/// (`ReconnectingChannel`) and a per-connection server (`H2Server`). A tonic
/// or axum service runs unchanged on [`TokioProviders`] and in the
/// simulator.
///
/// ```rust
/// use moonpool::hyper::{ChannelConfig, H2Server, KeepAlive, ReconnectingChannel};
/// ```
///
/// It is a namespaced module, not a third glob re-export at the root: the
/// core and simulation root exports already overlap in places, and names such
/// as `H2Channel` or `KeepAlive` read better qualified.
#[cfg(feature = "hyper")]
pub mod hyper {
    pub use moonpool_hyper::*;
}

/// Prometheus metrics in the simulation report, from
/// [`moonpool_prometheus`]. Requires the `prometheus` feature.
///
/// Give each simulated node a `PrometheusSource` with
/// `SimulationBuilder::metrics_factory`. The counters, gauges and histograms
/// the code already keeps appear in the report at the end of the run.
/// Metrics created through the source return instrumented handles that
/// record every change on the **simulated** clock, so the report holds an
/// exact time series. A final scrape reads everything else in the registry.
///
/// ```rust
/// use std::sync::Arc;
///
/// use moonpool::prometheus::PrometheusSource;
///
/// // One source per node, built fresh for every seed.
/// let factory = |_ip: &str| Arc::new(PrometheusSource::default());
/// let _node = factory("10.0.1.1");
/// ```
#[cfg(feature = "prometheus")]
pub mod prometheus {
    pub use moonpool_prometheus::*;
}

/// Typed dynamic-endpoint RPC, from [`moonpool_rpc`]. Requires the `rpc`
/// feature.
///
/// The module holds the `moonpool-rpc` runtime (`RpcDriver`, `RpcHandle`),
/// serialisable endpoint references, delivery modes, reply streams,
/// balancing, per-endpoint admission queues, security and graceful shutdown.
/// It runs over the same providers as everything else.
///
/// ```rust
/// use moonpool::rpc::{AccessClass, RpcConfig, RpcDriver};
/// ```
///
/// The optional parts have their own features. `rpc-derive` adds the
/// `#[moonpool_rpc::service]` attribute. Its generated code names the
/// `moonpool_rpc` crate, so a crate that uses it depends on `moonpool-rpc`
/// directly. `rpc-tls` adds the TLS session upgrade and `rpc-jwt` the JWT
/// verifier. Both are native-only.
#[cfg(feature = "rpc")]
pub mod rpc {
    pub use moonpool_rpc::*;
}

/// Common imports for application code.
///
/// ```rust
/// use moonpool::prelude::*;
/// ```
///
/// It brings the provider traits into scope. Their `async fn`s are callable
/// only with the trait in scope. It also brings [`Providers`] and, with the
/// `tokio` feature, [`TokioProviders`]. With the `sim` feature it adds the
/// simulation types a test needs: `Process`, `Workload`, `SimContext` and
/// `SimulationBuilder`.
pub mod prelude {
    pub use moonpool_core::prelude::*;

    #[cfg(feature = "sim")]
    pub use moonpool_sim::{Process, SimContext, SimulationBuilder, Workload};
}
