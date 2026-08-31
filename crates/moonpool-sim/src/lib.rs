//! # Moonpool Simulation Framework
//!
//! Deterministic simulation for testing distributed systems, inspired by
//! [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).
//!
//! ## Why Deterministic Simulation?
//!
//! `FoundationDB`'s insight: **bugs hide in error paths**. Production code rarely
//! exercises timeout handlers, retry logic, or failure recovery. Deterministic
//! simulation with fault injection finds these bugs before production does.
//!
//! Key properties:
//! - **Reproducible**: Same seed produces identical execution
//! - **Comprehensive**: Tests all failure modes (network, timing, corruption)
//! - **Fast**: Logical time skips idle periods
//!
//! ## Core Components
//!
//! - [`SimWorld`]: The simulation runtime managing events and time
//! - [`SimulationBuilder`]: Configure and run simulations
//! - [`chaos`]: Fault injection (buggify, 14 assertion macros, invariants)
//! - [`storage`]: Storage simulation with fault injection
//! - Multiverse exploration via `moonpool-explorer` (configured with [`ExplorationConfig`])
//!
//! ## Fault Injection Overview
//!
//! See [`chaos`] module for detailed documentation.
//!
//! | Mechanism | Default | What it tests |
//! |-----------|---------|---------------|
//! | TCP latencies | 1-11ms connect | Async scheduling |
//! | Random connection close | 0.001% | Reconnection, redelivery |
//! | Bit flip corruption | 0.01% | Checksum validation |
//! | Connect failure | 50% probabilistic | Timeout handling, retries |
//! | Clock drift | 100ms max | Leases, heartbeats |
//! | Buggified delays | 25% | Race conditions |
//! | Partial writes | 1000 bytes max | Message fragmentation |
//! | Packet loss | disabled | At-least-once delivery |
//! | Network partitions | disabled | Split-brain handling |
//! | Storage corruption | configurable | Checksum validation, recovery |
//! | Torn writes | configurable | Write atomicity, journaling |
//! | Sync failures | configurable | Durability guarantees |
//!
//! ## Frontier-Based Multi-Seed Exploration
//!
//! Exploration retains replay recipes for globally new assertion outcomes.
//! Productive timelines enqueue bounded continuations that replay to the last
//! discovery and then diverge with fresh deterministic randomness. Assertion
//! novelty and code-coverage history are cumulative across root seeds, so a
//! seed that discovers nothing new stops after its root timeline.
//!
//! Workers are short-lived child processes, bounded by `workers`; set it to
//! zero for sequential, fork-free exploration. Every worker reconstructs the
//! timeline from its root seed and recipe rather than snapshotting a live
//! simulation.
//!
//! ```ignore
//! SimulationBuilder::new()
//!     .enable_exploration(ExplorationConfig {
//!         workers: 0,
//!         max_runs_per_seed: 8_000,
//!         branching_factor: 4,
//!         max_frontier: 1_024,
//!         max_recipe_len: 64,
//!     })
//!     .until_coverage_stable(10, 1_000)
//!     .workload_factory(|| Box::new(MyWorkload::default()))
//!     .run();
//! ```

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

// Re-export core types for convenience
pub use moonpool_core::{
    Detach, NetworkProvider, Providers, RandomProvider, SimulationError, SimulationResult,
    TaskProvider, TcpListenerTrait, TimeError, TimeProvider,
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
    TokioNetworkProvider, TokioProviders, TokioTaskProvider, TokioTimeProvider,
};

// =============================================================================
// Core Modules
// =============================================================================

/// Core simulation engine for deterministic testing.
pub mod sim;

/// Failure-domain locality vocabulary shared by the engine and the runner.
pub mod locality;

/// The deterministic single-threaded executor (seeded-random scheduling).
pub mod executor;

/// Simulation runner and orchestration framework.
pub mod runner;

/// Chaos testing infrastructure for deterministic fault injection.
pub mod chaos;

/// Provider implementations for simulation.
pub mod providers;

/// Network simulation and configuration.
pub mod network;

/// Production-friendly observability layer (replaces legacy Timeline + Invariant).
pub mod observability;

/// Storage simulation and configuration.
pub mod storage;

/// Simulation workloads and binary targets.
pub mod simulations;

// =============================================================================
// Public API Re-exports
// =============================================================================

// Sim module re-exports
pub use network::sim::NetworkEvent;
pub use sim::{
    Event, ProcessKillKind, ScheduleError, ScheduleId, Scheduled, Scheduler, SimFaultRecord,
    SimWorld, SleepFuture, StorageOperation, WeakSimWorld, clear_rng_breakpoints, current_sim_seed,
    reset_rng_call_count, reset_sim_rng, rng_call_count, set_rng_breakpoints, set_sim_seed,
    set_swarm_op_seed, sim_random, sim_random_range, sim_random_range_or_default, swarm_op_enabled,
};

// Locality vocabulary re-exports (shared by engine and runner)
pub use locality::{DomainLevel, LinkClass, LocalityInfo};

// Runner module re-exports
pub use runner::{
    Attrition, AttritionScope, Chaos, ChaosMode, ClientId, FaultContext, FaultInjector,
    INSTANCE_LABEL, IterationControl, LocalityConfig, MachineRegistry, MetricsHandle, Process,
    ProcessTags, RebootKind, SimContext, SimulationBuilder, SimulationMetrics, SimulationReport,
    TagRegistry, Workload, WorkloadCount, WorkloadTopology,
};

// Application-metrics vocabulary, re-exported from moonpool-core so a
// simulation using `.metrics_factory()` needs one import path. Adapters over a
// concrete registry (moonpool-prometheus, and OpenTelemetry later) implement
// `MetricsSource` against these types.
pub use moonpool_core::metrics::{
    HistogramValue, MetricClock, MetricPoint, MetricSample, MetricValue, MetricsSource,
    SeriesRecorder,
};

// The metric query/report vocabulary, so a runner declaring
// `.metric(MetricQuery::select(...))` imports from one place. `Min`, `Mean`,
// `Max` and `Percentile` are the aggregator markers the builder accepts;
// `Percentile` deliberately only applies where the observations survive.
pub use moonpool_core::metrics::query::{
    Aggregator, Max, Mean, MetricQuery, MetricQueryPlan, MetricQueryReport, MetricQueryRow,
    MetricSnapshot, MetricWindowSummary, Min, Percentile, Provenance, SeriesKey,
};

// Buggify macros live in the standalone zero-dependency moonpool-buggify
// crate; re-exported here so existing `moonpool_sim::buggify!` call sites keep
// working and share the same state as direct moonpool-buggify users.
pub use moonpool_buggify::{buggify, buggify_with_prob};

// Chaos module re-exports
pub use chaos::{
    AssertionStats, SIM_FAULT_EVENT_NAME, SimFaultEvent, StateHandle, assertion_results,
    buggify_init, buggify_reset, has_always_violations, reset_always_violations,
    reset_assertion_results, validate_assertion_contracts,
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
    InMemoryStorage, SECTOR_SIZE, SectorBitSet, SimStorageProvider, StorageConfiguration,
    StorageError,
};

// Block-device simulation exports
pub use storage::{
    BlockCrashOutcome, BlockCrashReport, BlockEligibilityMask, BlockFaultConfig, BlockFaultKind,
    BlockFaultRecord, BlockSectorResolution, EioTarget, SimBlockDevice, SimBlockDeviceProvider,
    SimBlockStore,
};

// Provider exports
pub use providers::{SimProviders, SimRandomProvider, SimTaskProvider, SimTimeProvider};

// Assertion vocabulary — always available (dependency-free accounting layer).
pub use moonpool_assertions::{AssertCmp, AssertKind};
// Exploration-only re-exports (fork-based multiverse engine).
#[cfg(feature = "exploration")]
pub use moonpool_explorer::{ExplorationConfig, Recipe, format_timeline, parse_timeline};
pub use runner::report::{BugRecipe, ExplorationReport, MetricAggregate};

// Macros are automatically available at crate root when defined with #[macro_export]
