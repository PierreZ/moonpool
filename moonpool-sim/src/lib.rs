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
//! - [`chaos`]: Fault injection (buggify, assertions, invariants)
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

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

// Re-export core types for convenience
pub use moonpool_core::{
    CodecError, Endpoint, JsonCodec, MessageCodec, NetworkAddress, NetworkAddressParseError,
    NetworkProvider, Providers, RandomProvider, SimulationError, SimulationResult, TaskProvider,
    TcpListenerTrait, TimeProvider, TokioNetworkProvider, TokioProviders, TokioTaskProvider,
    TokioTimeProvider, UID, WELL_KNOWN_RESERVED_COUNT, WellKnownToken,
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
    SleepFuture, StorageOperation, WeakSimWorld, get_current_sim_seed, reset_sim_rng, set_sim_seed,
    sim_random, sim_random_range, sim_random_range_or_default,
};

// Runner module re-exports
pub use runner::{
    IterationControl, SimulationBuilder, SimulationMetrics, SimulationReport, TokioReport,
    TokioRunner, WorkloadTopology,
};

// Chaos module re-exports
pub use chaos::{
    AssertionStats, InvariantCheck, StateRegistry, buggify_init, buggify_reset,
    get_assertion_results, panic_on_assertion_violations, reset_assertion_results,
    validate_assertion_contracts,
};

// Network exports
pub use network::{
    ChaosConfiguration, ConnectFailureMode, NetworkConfiguration, SimNetworkProvider,
    sample_duration,
};

// Storage exports
pub use storage::{InMemoryStorage, SECTOR_SIZE, SectorBitSet, StorageConfiguration};

// Provider exports
pub use providers::{SimProviders, SimRandomProvider, SimTimeProvider};

// Macros are automatically available at crate root when defined with #[macro_export]
