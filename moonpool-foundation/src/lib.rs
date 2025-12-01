//! # Moonpool Simulation Framework
//!
//! A deterministic simulation framework for testing distributed systems,
//! inspired by FoundationDB's simulation testing approach.
//!
//! ## Key Features
//!
//! - **Deterministic execution**: Same seed produces identical behavior
//! - **Fault injection**: Comprehensive chaos testing via [`buggify!`] macros
//! - **Network simulation**: TCP-level faults, partitions, and latency
//! - **Time control**: Logical time with clock drift simulation
//!
//! ## Fault Injection
//!
//! Moonpool provides extensive fault injection following FDB's buggify patterns.
//!
//! Quick overview of chaos mechanisms:
//!
//! | Mechanism | Default | What it tests |
//! |-----------|---------|---------------|
//! | TCP operation latencies | 1-11ms connect | Async scheduling |
//! | Random connection close | 0.001% | Reconnection, redelivery |
//! | Bit flip corruption | 0.01% | Checksum validation |
//! | Connect failure | Mode 2 | Timeout handling, retries |
//! | Clock drift | 100ms max | Leases, heartbeats |
//! | Buggified delays | 25% | Race conditions |
//! | Partial writes | 1000 bytes | Message fragmentation |
//! | Network partitions | disabled | Split-brain handling |
//!
//! Configure via [`ChaosConfiguration`] and [`NetworkConfiguration`], or use defaults.
//!
//! ## Getting Started
//!
//! ```ignore
//! use moonpool_foundation::{SimulationBuilder, WorkloadTopology};
//!
//! SimulationBuilder::new()
//!     .topology(WorkloadTopology::ClientServer { clients: 2, servers: 1 })
//!     .run(|ctx| async move {
//!         // Your workload here
//!     });
//! ```

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

// =============================================================================
// Core Modules
// =============================================================================

/// Core simulation engine for deterministic testing.
pub mod sim;

/// Simulation runner and orchestration framework.
pub mod runner;

/// Chaos testing infrastructure for deterministic fault injection.
pub mod chaos;

/// Error types and utilities for simulation operations.
pub mod error;

// =============================================================================
// Provider Modules
// =============================================================================

/// Time provider abstraction for simulation and real time.
pub mod time;

/// Task spawning abstraction for single-threaded environments.
pub mod task;

/// Random number generation provider abstraction.
pub mod random;

// =============================================================================
// Network Module
// =============================================================================

/// Network simulation and abstraction layer.
pub mod network;

/// FDB-compatible messaging types and wire format.
pub mod messaging;

// =============================================================================
// Public API Re-exports
// =============================================================================

// Error types
pub use error::{SimulationError, SimulationResult};

// Sim module re-exports
pub use sim::{
    ConnectionStateChange, Event, EventQueue, NetworkOperation, ScheduledEvent, SimWorld,
    SleepFuture, WeakSimWorld, get_current_sim_seed, reset_sim_rng, set_sim_seed, sim_random,
    sim_random_range, sim_random_range_or_default,
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
    ChaosConfiguration, ConnectFailureMode, NetworkConfiguration, NetworkProvider, Peer,
    PeerConfig, PeerError, PeerMetrics, PeerReceiver, SimNetworkProvider, TcpListenerTrait,
    TokioNetworkProvider, sample_duration,
};

// Time provider exports
pub use time::{SimTimeProvider, TimeProvider, TokioTimeProvider};

// Task provider exports
pub use task::{TaskProvider, TokioTaskProvider};

// Random provider exports
pub use random::{RandomProvider, SimRandomProvider};

// FDB-compatible messaging exports
pub use messaging::{
    Endpoint, HEADER_SIZE, MAX_PAYLOAD_SIZE, NetworkAddress, NetworkAddressParseError,
    PacketHeader, UID, WELL_KNOWN_RESERVED_COUNT, WellKnownToken, WireError, deserialize_packet,
    flags, serialize_packet, try_deserialize_packet,
};

// Macros are automatically available at crate root when defined with #[macro_export]
