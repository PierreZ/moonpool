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
//! See the [`fault_injection`] module for complete documentation.
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

/// Assertion macros and result tracking for simulation testing.
pub mod assertions;
/// Buggify system for deterministic chaos testing.
pub mod buggify;
/// Error types and utilities for simulation operations.
pub mod error;
/// Comprehensive guide to fault injection mechanisms.
pub mod fault_injection;
/// Event scheduling and processing for the simulation engine.
pub mod events;
/// Invariant checking for cross-workload validation.
pub mod invariants;
/// FDB-compatible messaging types and wire format.
pub mod messaging;
/// Network simulation and abstraction layer.
pub mod network;
/// Network state management for simulation.
mod network_state;
/// Random number generation provider abstraction.
pub mod random;
/// Thread-local random number generation for simulation.
pub mod rng;
/// Simulation runner and statistical analysis framework.
pub mod runner;
/// Core simulation world and coordination logic.
pub mod sim;
/// Sleep functionality for simulation time.
pub mod sleep;
/// State registry for actor observability and debugging.
pub mod state_registry;
/// Task provider abstraction for spawning local tasks.
pub mod task;
/// Time provider abstraction for simulation and real time.
pub mod time;
/// Tokio-based runner for real-world execution.
pub mod tokio_runner;

// Public API exports
pub use assertions::{AssertionStats, get_assertion_results, validate_assertion_contracts};
pub use buggify::{buggify_init, buggify_reset};
pub use error::{SimulationError, SimulationResult};
pub use events::{ConnectionStateChange, Event, EventQueue, NetworkOperation, ScheduledEvent};
pub use invariants::InvariantCheck;
pub use state_registry::StateRegistry;
// Network exports
pub use network::{
    ChaosConfiguration, NetworkConfiguration, NetworkProvider, Peer, PeerConfig, PeerError,
    PeerMetrics, SimNetworkProvider, TcpListenerTrait, TokioNetworkProvider, sample_duration,
};
// Random provider exports
pub use random::{RandomProvider, sim::SimRandomProvider};
// Time provider exports
pub use rng::{
    get_current_sim_seed, reset_sim_rng, set_sim_seed, sim_random, sim_random_range,
    sim_random_range_or_default,
};
pub use runner::{SimulationBuilder, SimulationMetrics, SimulationReport, WorkloadTopology};
pub use sim::{SimWorld, WeakSimWorld};
pub use sleep::SleepFuture;
pub use task::{TaskProvider, tokio_provider::TokioTaskProvider};
pub use time::{SimTimeProvider, TimeProvider, TokioTimeProvider};
pub use tokio_runner::{TokioReport, TokioRunner};
// FDB-compatible messaging exports
pub use messaging::{
    Endpoint, HEADER_SIZE, MAX_PAYLOAD_SIZE, NetworkAddress, NetworkAddressParseError,
    PacketHeader, UID, WELL_KNOWN_RESERVED_COUNT, WellKnownToken, WireError, deserialize_packet,
    flags, serialize_packet, try_deserialize_packet,
};

// Macros are automatically available at crate root when defined with #[macro_export]
