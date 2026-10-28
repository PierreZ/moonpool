//! # moonpool-sim
//!
//! Deterministic simulation for distributed systems, after
//! [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).
//!
//! moonpool-sim runs a whole cluster (server processes, test clients, the
//! network between them and the disks under them) in one OS thread, on
//! logical time, from one seed. The same seed gives the same run, bit for
//! bit. Idle time costs nothing: a simulated hour of heartbeats passes in
//! milliseconds. While the code runs, the simulator injects faults: lost and
//! corrupted packets, partitions, torn and lost writes, process crashes,
//! power outages, slow machines. Assertions record what happened, and the
//! runner keeps trying new seeds until every "sometimes" assertion fired and
//! coverage stops growing.
//!
//! The code under test is **the code you ship**. It depends on the provider
//! traits of `moonpool-core` ([`TimeProvider`], [`TaskProvider`],
//! [`NetworkProvider`], [`RandomProvider`], [`StorageProvider`],
//! [`SystemProvider`]). Production passes `TokioProviders`; the simulation
//! passes [`SimProviders`]. Nothing is wrapped in a test-only variant.
//!
//! # Quick start
//!
//! A simulation has three parts:
//!
//! - a [`Process`]: the system under test, booted fresh on every (re)boot;
//! - a [`Workload`]: the test driver, which survives process reboots;
//! - a [`SimulationBuilder`]: the run configuration (topology, chaos,
//!   seeds), which [`run`](SimulationBuilder::run)s and returns a
//!   [`SimulationReport`].
//!
//! ```rust
//! use std::time::Duration;
//!
//! use async_trait::async_trait;
//! use futures::io::{AsyncReadExt, AsyncWriteExt};
//! use moonpool_sim::{
//!     NetworkProvider, Process, SimContext, SimulationBuilder, SimulationResult,
//!     TcpListenerTrait, TimeProvider, Workload, assert_always, assert_sometimes,
//! };
//!
//! /// The system under test: an echo server.
//! struct Echo;
//!
//! #[async_trait]
//! impl Process for Echo {
//!     fn name(&self) -> &str {
//!         "echo"
//!     }
//!
//!     async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
//!         let listener = ctx.network().bind(ctx.my_ip()).await?;
//!         loop {
//!             // moonpool_sim::select!, not tokio::select!: the branch order
//!             // is a seeded draw, so it replays.
//!             let accepted = moonpool_sim::select! {
//!                 accepted = listener.accept() => accepted,
//!                 () = ctx.shutdown().cancelled() => return Ok(()),
//!             };
//!             let Ok((mut stream, _peer)) = accepted else { continue };
//!             let mut buf = [0u8; 4];
//!             if stream.read_exact(&mut buf).await.is_ok() {
//!                 let _ = stream.write_all(&buf).await;
//!             }
//!         }
//!     }
//! }
//!
//! /// The test driver: send one message and check the echo.
//! struct Client;
//!
//! #[async_trait]
//! impl Workload for Client {
//!     fn name(&self) -> &str {
//!         "client"
//!     }
//!
//!     async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
//!         let server = ctx.topology().ips_in_group("echo")[0].clone();
//!         // The default network can refuse a connect, or hang it forever, as
//!         // FoundationDB's simulator does. Bound every exchange with a
//!         // simulated timeout and retry.
//!         let reply = loop {
//!             let exchange = async {
//!                 let mut stream = ctx.network().connect(&server).await?;
//!                 stream.write_all(b"ping").await?;
//!                 let mut reply = [0u8; 4];
//!                 stream.read_exact(&mut reply).await?;
//!                 Ok::<_, std::io::Error>(reply)
//!             };
//!             match ctx.time().timeout(Duration::from_secs(1), exchange).await {
//!                 Ok(Ok(reply)) => break reply,
//!                 _ => {
//!                     let _ = ctx.time().sleep(Duration::from_millis(10)).await;
//!                 }
//!             }
//!         };
//!         // A safety property: checked every time, never panics.
//!         assert_always!(&reply == b"ping", "echo returns the bytes it got");
//!         // A coverage goal: must fire on at least one seed.
//!         assert_sometimes!(true, "a client got its echo");
//!         Ok(())
//!     }
//! }
//!
//! let report = SimulationBuilder::new()
//!     .processes(1, || Box::new(Echo))
//!     .workload_factory(|| Box::new(Client))
//!     .set_debug_seeds(vec![1, 2, 3])
//!     .set_iterations(3)
//!     .run()
//!     .expect("the builder is valid");
//! assert!(!report.is_failure(), "{report}");
//! ```
//!
//! To find bugs, turn on chaos and let the runner pick the seeds:
//!
//! ```rust
//! use std::time::Duration;
//! use moonpool_sim::{
//!     Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimulationBuilder,
//! };
//!
//! // At most one process down at a time; crashes most of the time.
//! let reboots = Attrition {
//!     max_dead: 1,
//!     prob_graceful: 0.2,
//!     prob_crash: 0.7,
//!     prob_wipe: 0.1,
//!     recovery_delay_ms: None,
//!     grace_period_ms: None,
//!     scope: AttritionScope::PerProcess,
//!     victims: AttritionVictims::Any,
//!     wipe_budget: None,
//! };
//! let builder = SimulationBuilder::new()
//!     .enable_chaos([
//!         Chaos::Network(ChaosMode::Swarm),
//!         Chaos::Storage(ChaosMode::Swarm),
//!         Chaos::Attrition { config: reboots, mode: ChaosMode::Swarm },
//!         Chaos::BuggifyKnobs,
//!     ])
//!     // Faults are injected for 30 simulated seconds, then the system
//!     // gets a quiet tail to recover in.
//!     .chaos_duration(Duration::from_secs(30))
//!     // The default stop rule, written out.
//!     .until_coverage_stable(10, 1_000);
//! # let _ = builder;
//! ```
//!
//! A failing seed is in [`SimulationReport::seeds_failing`]. Pin it with
//! [`set_debug_seeds`](SimulationBuilder::set_debug_seeds) and
//! [`set_iterations(1)`](SimulationBuilder::set_iterations), raise
//! [`trace_level`](SimulationBuilder::trace_level), and it replays exactly.
//!
//! # The life of one seed
//!
//! 1. The runner seeds the one random stream ([`sim::rng`]) and samples each
//!    enabled chaos surface for this seed.
//! 2. Process groups boot: each [`processes`](SimulationBuilder::processes)
//!    or [`cluster`](SimulationBuilder::cluster) group draws its size and
//!    takes its own `10.0.{group}.x` range. Workloads live on `10.0.0.x`.
//! 3. Every workload runs [`Workload::setup`], then all [`Workload::run`]s
//!    run together.
//! 4. During [`chaos_duration`](SimulationBuilder::chaos_duration), fault
//!    injectors run beside the workloads. When it ends, the runner calls
//!    [`SimWorld::enter_recovery_mode`]: no **new** faults, the damage done
//!    stays.
//! 5. When the first workload returns from `run`, the shutdown token
//!    ([`SimContext::shutdown`]) fires for every actor. When all workloads
//!    have returned, processes stop, pending events drain, and every
//!    [`Workload::check`] runs. [`Invariant`]s run after every step.
//! 6. The seed fails on a workload error or panic, an `assert_always!`
//!    violation, an invariant panic, or a deadlock.
//!
//! # Map of the crate
//!
//! ## Runner and builder
//!
//! | Item | Use it to |
//! |------|-----------|
//! | [`SimulationBuilder`] | Configure and run a campaign of seeds |
//! | [`Process`], [`RebootKind`] | Write the system under test and say how it reboots |
//! | [`Workload`], [`WorkloadCount`] | Write the test driver; spawn one or many clients |
//! | [`SimContext`] | Reach providers, topology, shared state and shutdown from inside the simulation |
//! | [`StateHandle`] | Share typed values between processes, workloads and the harness |
//! | [`WorkloadTopology`], [`ProcessTags`], [`GroupRegistry`] | Find peers by name, group or tag |
//! | [`LocalityConfig`], [`MachineRegistry`], [`LocalityInfo`] | Lay processes out over datacenters, zones and machines |
//! | [`IterationControl`] | Stop after N seeds or when coverage is stable |
//! | [`SimulationReport`], [`SimulationMetrics`] | Read the result of a campaign |
//!
//! ## Chaos
//!
//! [`SimulationBuilder::enable_chaos`] takes a list of [`Chaos`] surfaces.
//! Each surface has a [`ChaosMode`]: [`Random`](ChaosMode::Random) turns the
//! whole surface on with per-seed intensities; [`Swarm`](ChaosMode::Swarm)
//! turns on a per-seed random **subset** of its fault families, so rare
//! single-fault configurations happen often.
//!
//! | Surface | What it does |
//! |---------|--------------|
//! | [`Chaos::Network`] | Partitions, clogs, bit flips, random closes, connect failures, clock drift, black holes ([`NetworkConfiguration`], [`ChaosConfiguration`]) |
//! | [`Chaos::Storage`] | Corruption, `EIO`, misdirected and phantom writes, sync failures, crash damage to unsynced sectors, disk stalls, disk failure ([`StorageConfiguration`]) |
//! | [`Chaos::Attrition`] | Kills and restarts processes under a `max_dead` budget ([`Attrition`], [`AttritionScope`], [`AttritionVictims`]) |
//! | [`Chaos::Outage`] | A correlated power loss of whole process groups ([`Outage`]); the harness may defer it with an [`OutageVeto`] |
//! | [`Chaos::Cpu`] | The CPU gate: every poll of a process costs CPU time on its one core ([`CpuModel`]) |
//! | [`Chaos::GrayFailure`] | Machines that stay alive but slow for a long window ([`GrayFailure`], [`Slowness`]) |
//! | [`Chaos::BuggifyKnobs`] | Pushes enabled network and storage knobs to extremes on some seeds |
//!
//! A surface that is off is not fault-free. Buggify sites fire on every
//! seed, and without [`Chaos::Network`] the network uses
//! [`NetworkConfiguration::default`], which keeps `FoundationDB`'s
//! always-on behavior: rare random closes, clock drift, buggified delays,
//! and buggified connects that fail or hang forever. Bound every network
//! call in a workload with a simulated timeout, as the quick start does.
//!
//! More controls:
//!
//! - **Swarm masks.** [`network_fault_mask`](SimulationBuilder::network_fault_mask)
//!   and [`storage_fault_mask`](SimulationBuilder::storage_fault_mask) remove
//!   fault families ([`NetworkFault`], [`StorageFault`]) a harness cannot
//!   judge, without changing any draw.
//!   [`swarm_operations`](SimulationBuilder::swarm_operations) does the same
//!   for the workload's own operations ([`swarm_op_enabled`]).
//! - **Aimed storage faults.** [`replicated_storage_faults`](SimulationBuilder::replicated_storage_faults)
//!   spreads damage over failure domains ([`ReplicatedFaults`],
//!   [`FaultPattern`]) so a replicated record is never damaged everywhere.
//!   [`FaultFocus`] weighs damage toward byte ranges a process publishes as
//!   [`LayoutRegion`]s.
//! - **Custom fault injectors.** [`fault_factory`](SimulationBuilder::fault_factory)
//!   registers a [`FaultInjector`]. Its [`FaultContext`] can partition, crash,
//!   restart, corrupt bytes, read the [`durable_bytes`](FaultContext::durable_bytes)
//!   of a file and slow a machine.
//! - **Self-crash.** A process crashes itself at an exact point of its
//!   protocol with [`SimContext::crash_self`] or a [`SelfCrash`] handle.
//! - **Slowness.** Workloads read a machine's gray-failure factors with
//!   [`SimContext::slowness`].
//!
//! ## Fault sites in the code under test
//!
//! Shipped code marks its own interesting moments. Outside a simulation
//! every one of these is a no-op.
//!
//! - [`buggify!`], [`buggify_with_prob!`]: take a rare path. Each site is
//!   activated once per seed, then fires at its rate.
//! - [`buggify_fault_with_prob!`]: a site that makes an operation fail. It
//!   is silent after the chaos window.
//! - [`buggify_named!`] and [`set_activation`]: a site with a stable label,
//!   so a harness can turn it on or off per seed.
//! - [`buggify_pick!`], [`buggify_range!`], [`buggify_knob!`]: override a
//!   choice or a configuration value.
//! - [`hint!`]: names a moment ("mid-append", "after the vote"). The
//!   simulator's hint sink may reboot the calling process there, under the
//!   seed's attrition regime and its `max_dead` budget. A [`HintVeto`] adds
//!   a budget of the harness's own.
//! - [`cpu_charge`]: tells the CPU gate that a piece of work costs CPU time.
//!
//! ## Assertions
//!
//! Fifteen Antithesis-style macros (see [`chaos::assertions`]). They never
//! panic: a failed `always` is recorded and fails the seed at the end.
//!
//! - Boolean: [`assert_always!`], [`assert_always_or_unreachable!`],
//!   [`assert_sometimes!`], [`assert_reachable!`], [`assert_unreachable!`].
//! - Numeric: [`assert_always_greater_than!`],
//!   [`assert_always_greater_than_or_equal_to!`], [`assert_always_less_than!`],
//!   [`assert_always_less_than_or_equal_to!`], [`assert_sometimes_greater_than!`],
//!   [`assert_sometimes_greater_than_or_equal_to!`],
//!   [`assert_sometimes_less_than!`], [`assert_sometimes_less_than_or_equal_to!`].
//! - Compound: [`assert_sometimes_all!`], [`assert_sometimes_each!`].
//!
//! [`assertion_results`] and [`validate_assertion_contracts`] read the
//! counts; the report lists every assertion and every violation.
//!
//! ## Events and invariants
//!
//! Code emits ordinary `tracing` events. [`SimulationLayer`] captures them
//! into a timeline, and [`Invariant`]s (or
//! [`invariant_fn`](SimulationBuilder::invariant_fn)) read it through
//! [`TraceQuery`] after every simulation step. Injected faults appear on the
//! same timeline as [`SimFaultEvent`]s. See [`observability`].
//!
//! ## Network and storage simulation
//!
//! - [`network`]: [`SimNetworkProvider`], TCP-like streams with latency,
//!   send windows ([`tcp_send_window_bytes`](SimulationBuilder::tcp_send_window_bytes)),
//!   listener backlogs and distance-based latency
//!   ([`link_latency`](SimulationBuilder::link_latency)).
//! - [`storage`]: [`SimStorageProvider`] over one file image per file
//!   ([`FileImage`]), with a barrier-bounded crash model: at a crash, each
//!   sector written since the last sync resolves on its own to old, new, lost
//!   or damaged. A lost-synced-write oracle checks that nothing synced is
//!   lost. The engine records what it did in [`StorageFaultRecord`]s and
//!   [`FileCrashReport`]s.
//! - [`providers`]: [`SimProviders`] bundles the six simulated providers.
//!   [`ScriptedResolver`] answers name lookups from a table the simulation
//!   edits while it runs.
//!
//! ## Executor and `select!`
//!
//! [`executor`] is a single-threaded executor. When several tasks are ready,
//! it picks the next one with a seeded draw, so the seed space explores task
//! interleavings. [`select!`] is `tokio::select!` with a seeded start branch.
//! Use it instead of `tokio::select!` in every process and workload.
//!
//! ## Determinism tools
//!
//! - [`sim::rng`]: the one counted random stream ([`rng_call_count`],
//!   [`set_rng_breakpoints`]).
//! - [`check_determinism`](SimulationBuilder::check_determinism): runs each
//!   seed twice and fails it when the two runs draw differently.
//! - [`run_time_budget`](SimulationBuilder::run_time_budget): turns a
//!   timer that never stops into a deadlock failure.
//!
//! ## Exploration
//!
//! With the `exploration` feature (on by default),
//! [`enable_exploration`](SimulationBuilder::enable_exploration) turns on
//! `moonpool-explorer`. When a seed reaches a new assertion outcome, the
//! explorer records a recipe (RNG call counts and seeds) and replays it with
//! fresh randomness after that point, in bounded worker processes. Found
//! bugs come back as [`BugRecipe`]s; replay one with
//! [`replay_timeline`](SimulationBuilder::replay_timeline). Exploration
//! needs factory workloads ([`workload_factory`](SimulationBuilder::workload_factory)
//! or [`workloads`](SimulationBuilder::workloads)).
//!
//! ## Application metrics
//!
//! [`metrics_factory`](SimulationBuilder::metrics_factory) gives every node
//! its own [`MetricsSource`] (for example a `PrometheusSource` from
//! `moonpool-prometheus`), and [`metric`](SimulationBuilder::metric)
//! declares a [`MetricQuery`] the runner evaluates on every seed.
//!
//! # Feature flags
//!
//! | Feature | Default | Effect |
//! |---------|---------|--------|
//! | `tokio-providers` | on | Re-exports the production bundle (`TokioProviders` and its parts) so one test can drive the same code under both |
//! | `exploration` | on | Fork-based multiverse exploration through `moonpool-explorer` (uses `libc`, never on wasm) |
//!
//! With `--no-default-features` the crate builds for `wasm32`. Assertion
//! accounting, coverage-based stopping and every chaos surface still work.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

// Re-export core types for convenience
pub use moonpool_core::{
    Detach, DiskCounters, DiskSpace, LayoutRegion, NetworkProvider, OpenOptions, Providers,
    RandomProvider, Resolver, SimulationError, SimulationResult, StorageFile, StorageProvider,
    SystemProvider, SystemSample, TaskProvider, TcpListenerTrait, TimeError, TimeProvider,
};
// Local (non-`Send`) provider traits: the sim backends implement them too, so
// code written for a thread-per-core runtime runs under simulation.
pub use moonpool_core::{
    LocalNetworkProvider, LocalProviders, LocalStorageFile, LocalStorageProvider,
    LocalTaskProvider, LocalTcpListener, LocalTimeProvider,
};
// The deterministic select! (moonpool-sim always enables core's
// deterministic-select, so this is tokio's expansion with a seeded start
// offset). Process and
// workload code must use this instead of tokio::select!, whose branch offset is
// entropy-drawn outside a seeded tokio runtime.
pub use moonpool_core::select;
// Production provider bundle — only when the tokio-providers feature pulls core's
// net/fs/time. A wasm-able sim (`--no-default-features`) omits these.
#[cfg(feature = "tokio-providers")]
pub use moonpool_core::{
    TokioNetworkProvider, TokioProviders, TokioSystemProvider, TokioTaskProvider, TokioTimeProvider,
};

// =============================================================================
// Core Modules
// =============================================================================

pub mod sim;

pub mod locality;

pub mod executor;

pub mod runner;

pub mod chaos;

pub mod providers;

pub mod network;

pub mod observability;

pub mod storage;

// =============================================================================
// Public API Re-exports
// =============================================================================

// Sim module re-exports
pub use network::sim::NetworkEvent;
pub use sim::{
    CpuModel, DeterminismViolation, Event, ProcessKillKind, SYSTEM_SAMPLES_KEPT, ScheduleError,
    ScheduleId, Scheduled, Scheduler, SimFaultRecord, SimWorld, SleepFuture, Slowness,
    StorageOperation, WeakSimWorld, begin_determinism_check, begin_determinism_record,
    clear_rng_breakpoints, clear_swarm_op_mask, current_sim_seed, draw_swarm_op_mask,
    finish_determinism_check, reset_rng_call_count, reset_sim_rng, rng_call_count,
    set_rng_breakpoints, set_sim_seed, sim_random, sim_random_bool, sim_random_f64,
    sim_random_range, sim_random_range_or_default, stop_determinism_canary, swarm_op_enabled,
};

// Locality vocabulary re-exports (shared by engine and runner)
pub use locality::{DomainLevel, LinkClass, LocalityInfo};

// Runner module re-exports
pub use runner::{
    Aim, Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, Core, DEFAULT_MAX_SPAWNS,
    Deployment, DeploymentChanges, DomainSpec, Downgrade, FaultContext, FaultInjector, GrayFailure,
    GrayKinds, GroupRegistry, HINT_VETO_KEY, HintVeto, INSTANCE_LABEL, IterationControl,
    LocalityConfig, MachineRegistry, Membership, MetricsHandle, OUTAGE_STATE_KEY, OUTAGE_VETO_KEY,
    Outage, OutageLanded, OutageVeto, Process, ProcessTags, RebootKind, SHARD_BOOK_KEY, SelfCrash,
    ShardBook, ShardCounts, ShardEvent, ShardFaults, ShardState, ShardView, Shards, SimContext,
    SimulationBuilder, SimulationMetrics, SimulationReport, TIMED_REBOOT_LABEL, TagRegistry,
    VETO_RETRY, WIPE_LEDGER_KEY, WipeBudget, WipeHeal, WipeLedger, Workload, WorkloadCount,
    WorkloadTopology,
};

// Application-metrics vocabulary, re-exported from moonpool-core so a
// simulation using `.metrics_factory()` needs one import path. Adapters over a
// concrete registry (moonpool-prometheus, and OpenTelemetry later) implement
// `MetricsSource` against these types.
pub use moonpool_core::metrics::{
    HistogramValue, MetricClock, MetricPoint, MetricSample, MetricValue, MetricsSource,
    SeriesRecorder, f64_to_u64, u64_to_f64_exact,
};

// The metric query/report vocabulary, so a runner declaring
// `.metric(MetricQuery::select(...))` imports from one place. `Min`, `Mean`,
// `Max` and `Percentile` are the aggregator markers the builder accepts;
// `Percentile` deliberately only applies where the observations survive.
pub use moonpool_core::metrics::query::{
    Aggregator, Fill, Max, Mean, MetricQuery, MetricQueryPlan, MetricQueryReport, MetricQueryRow,
    MetricSnapshot, MetricWindowSummary, Min, Percentile, Provenance, SeriesKey, WHOLE_RUN_MS,
};

// Buggify macros live in the standalone zero-dependency moonpool-buggify
// crate; re-exported here so existing `moonpool_sim::buggify!` call sites keep
// working and share the same state as direct moonpool-buggify users.
pub use moonpool_buggify::{
    buggify, buggify_fault_with_prob, buggify_named, buggify_pick, buggify_range,
    buggify_with_prob, cpu_charge, hint, set_activation, shard,
};

// Chaos module re-exports
pub use chaos::{
    AssertionStats, SIM_FAULT_EVENT_NAME, SimFaultEvent, StateHandle, assertion_results,
    buggify_init, buggify_reset, chaos_ended_at, has_always_violations, in_chaos,
    reset_always_violations, reset_assertion_results, validate_assertion_contracts,
};

// Observability module re-exports (plain-tracing capture + invariants)
pub use observability::{
    Clock, FieldValue, InstallGuard, Invariant, SimTime, SimulationLayer, SimulationLayerHandle,
    TraceEvent, TraceQuery, init_sim_tracing, invariant_fn,
};

// Network exports
pub use network::{
    ChaosConfiguration, ConnectFailureMode, LatencyDistribution, LinkLatencyConfig,
    NetworkConfiguration, NetworkFault, NetworkFaultMask, PartitionStrategy, SimNetworkProvider,
    sample_duration, sample_latency,
};

// Storage exports
pub use storage::{
    CrashOutcome, DiskCapacity, EioTarget, FaultFocus, FaultPattern, FileCrashReport, FileImage,
    ReplicatedFaults, SECTOR_SIZE, SectorBitSet, SectorResolution, SimStorageProvider,
    StorageConfiguration, StorageEligibilityMask, StorageError, StorageFault, StorageFaultKind,
    StorageFaultMask, StorageFaultRecord, WritebackErrorReporting,
};

// Provider exports
pub use providers::{
    ScriptedResolver, SimProviders, SimRandomProvider, SimSystemProvider, SimTaskProvider,
    SimTimeProvider,
};

// Assertion vocabulary — always available (dependency-free accounting layer).
pub use moonpool_assertions::{AssertCmp, AssertKind, reachable, sometimes};
// Exploration-only re-exports (fork-based multiverse engine).
#[cfg(feature = "exploration")]
pub use moonpool_explorer::{ExplorationConfig, Recipe, format_timeline, parse_timeline};
pub use runner::report::{BugRecipe, ExplorationReport, MetricAggregate};

// Macros are automatically available at crate root when defined with #[macro_export]
