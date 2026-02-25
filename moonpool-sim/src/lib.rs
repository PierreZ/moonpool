//! # Moonpool Simulation Framework
//!
//! Deterministic simulation for testing distributed systems, inspired by
//! [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).
//!
//! ## Why Deterministic Simulation?
//!
//! FoundationDB's insight: **bugs hide in error paths**. Production code rarely
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
//! - Multiverse exploration via `moonpool-explorer` (re-exported as [`ExplorationConfig`], [`AdaptiveConfig`])
//!
//! ## Quick Start
//!
//! ```ignore
//! use moonpool_sim::{SimulationBuilder, WorkloadTopology};
//!
//! SimulationBuilder::new()
//!     .topology(WorkloadTopology::ClientServer { clients: 2, servers: 1 })
//!     .run(|ctx| async move {
//!         // Your distributed system workload
//!     });
//! ```
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
//! ## Multi-Seed Testing
//!
//! Tests run across multiple seeds to explore the state space:
//!
//! ```ignore
//! SimulationBuilder::new()
//!     .run_count(IterationControl::UntilAllSometimesReached(1000))
//!     .run(workload);
//! ```
//!
//! Debugging a failing seed:
//!
//! ```ignore
//! SimulationBuilder::new()
//!     .set_seed(failing_seed)
//!     .run_count(IterationControl::FixedCount(1))
//!     .run(workload);
//! ```
//!
//! ## Coverage-Preserving Multi-Seed Exploration
//!
//! When exploration is enabled, multiple seeds share coverage context. The
//! explored map (coverage bitmap union) and assertion watermarks are preserved
//! between seeds so each subsequent seed focuses energy on genuinely new
//! branches rather than re-treading already-discovered paths.
//!
//! A **warm start** mechanism reduces wasted forks: on seeds after the first,
//! marks whose first probe batch finds zero new coverage bits stop after
//! `warm_min_timelines` forks instead of the full `min_timelines`.
//!
//! ```text
//!  Seed 1 (cold start)           Seed 2 (warm start)          Seed 3 (warm start)
//!  energy: 400K                  energy: 400K                 energy: 400K
//!
//!  root ─┬─ mark A ──> 400       root ─┬─ mark A ──> 30      root ─┬─ mark A ──> 30
//!        │  (new bits!) forks           │  (barren!) skip           │  (barren!) skip
//!        │                              │                           │
//!        ├─ mark B ──> 400              ├─ mark B ──> 30            ├─ mark B ──> 30
//!        │  (new bits!) forks           │  (barren!) skip           │  (barren!) skip
//!        │                              │                           │
//!        └─ mark C ──> 400              ├─ mark C ──> 30            ├─ mark C ──> 30
//!           (new bits!) forks           │  (barren!) skip           │  (barren!) skip
//!                                       │                           │
//!                                       └─ mark D ──> 400          └─ mark E ──> 400
//!                                          (NEW bits!) forks           (NEW bits!) forks
//!                        │                               │                            │
//!       ┌────────────────┘              ┌────────────────┘           ┌────────────────┘
//!       v                               v                            v
//!  ┌──────────┐  ──preserved──>  ┌──────────┐  ──preserved──>  ┌──────────┐
//!  │ explored │  coverage map    │ explored │  coverage map    │ explored │
//!  │ map:     │  + watermarks    │ map:     │  + watermarks    │ map:     │
//!  │ A,B,C    │                  │ A,B,C,D  │                  │ A,B,C,D,E│
//!  └──────────┘                  └──────────┘                  └──────────┘
//!
//!  Total: each seed spends most energy on NEW discoveries.
//!  Warm marks (A,B,C on seed 2) exit after warm_min_timelines (30)
//!  instead of min_timelines (400), saving ~95% energy per barren mark.
//! ```
//!
//! ```ignore
//! SimulationBuilder::new()
//!     .set_iterations(3)  // 3 root seeds with coverage forwarding
//!     .enable_exploration(ExplorationConfig {
//!         max_depth: 120,
//!         timelines_per_split: 4,
//!         global_energy: 400_000,  // per-seed energy budget
//!         adaptive: Some(AdaptiveConfig {
//!             batch_size: 30,
//!             min_timelines: 400,
//!             max_timelines: 1_000,
//!             per_mark_energy: 10_000,
//!             warm_min_timelines: Some(30),  // quick skip for barren warm marks
//!         }),
//!         parallelism: Some(Parallelism::HalfCores),
//!     })
//!     .workload(my_workload);
//! ```

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

// Re-export core types for convenience
pub use moonpool_core::{
    CodecError, Endpoint, JsonCodec, MessageCodec, NetworkAddress, NetworkAddressParseError,
    NetworkProvider, Providers, RandomProvider, SimulationError, SimulationResult, TaskProvider,
    TcpListenerTrait, TimeError, TimeProvider, TokioNetworkProvider, TokioProviders,
    TokioTaskProvider, TokioTimeProvider, UID, WELL_KNOWN_RESERVED_COUNT, WellKnownToken,
};

// =============================================================================
// Core Modules
// =============================================================================

/// Core simulation engine for deterministic testing.
pub mod sim;

/// Simulation runner and orchestration framework.
pub mod runner;

/// Chaos testing infrastructure for deterministic fault injection.
pub mod chaos;

/// Provider implementations for simulation.
pub mod providers;

/// Network simulation and configuration.
pub mod network;

/// Storage simulation and configuration.
pub mod storage;

// =============================================================================
// Public API Re-exports
// =============================================================================

// Sim module re-exports
pub use sim::{
    ConnectionStateChange, Event, EventQueue, NetworkOperation, ScheduledEvent, SimWorld,
    SleepFuture, StorageOperation, WeakSimWorld, clear_rng_breakpoints, get_current_sim_seed,
    get_rng_call_count, reset_rng_call_count, reset_sim_rng, set_rng_breakpoints, set_sim_seed,
    sim_random, sim_random_range, sim_random_range_or_default,
};

// Runner module re-exports
pub use runner::{
    ClientId, FaultContext, FaultInjector, IterationControl, PhaseConfig, SimContext,
    SimulationBuilder, SimulationMetrics, SimulationReport, TokioReport, TokioRunner, Workload,
    WorkloadCount, WorkloadTopology,
};

// Chaos module re-exports
pub use chaos::{
    AssertionStats, Invariant, StateHandle, buggify_init, buggify_reset, get_assertion_results,
    has_always_violations, invariant_fn, panic_on_assertion_violations, reset_always_violations,
    reset_assertion_results, validate_assertion_contracts,
};

// Network exports
pub use network::{
    ChaosConfiguration, ConnectFailureMode, NetworkConfiguration, SimNetworkProvider,
    sample_duration,
};

// Storage exports
pub use storage::{
    InMemoryStorage, SECTOR_SIZE, SectorBitSet, SimStorageProvider, StorageConfiguration,
};

// Provider exports
pub use providers::{SimProviders, SimRandomProvider, SimTimeProvider};

// Explorer re-exports
pub use moonpool_explorer::{
    AdaptiveConfig, AssertCmp, AssertKind, ExplorationConfig, Parallelism, format_timeline,
    parse_timeline,
};
pub use runner::report::{BugRecipe, ExplorationReport};

// Macros are automatically available at crate root when defined with #[macro_export]
