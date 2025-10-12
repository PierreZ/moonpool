//! # Moonpool Simulation Framework
//!
//! A deterministic simulation framework for testing distributed systems.
//!
//! ## Phase 1: Core Infrastructure
//!
//! This phase provides the foundation for the simulation framework:
//! - Logical time engine with event-driven time advancement
//! - Event queue and processing for deterministic execution
//! - Basic simulation harness with centralized state management
//! - Handle pattern for avoiding borrow checker conflicts
//!
//! Provides SimWorld for deterministic simulation and event processing.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

/// Assertion macros and result tracking for simulation testing.
pub mod assertions;
/// Buggify system for deterministic chaos testing.
pub mod buggify;
/// Error types and utilities for simulation operations.
pub mod error;
/// Event scheduling and processing for the simulation engine.
pub mod events;
/// Invariant checking for cross-workload validation.
pub mod invariants;
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
    NetworkConfiguration, NetworkProvider, Peer, PeerConfig, PeerError, PeerMetrics,
    SimNetworkProvider, TcpListenerTrait, TokioNetworkProvider, sample_duration,
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

// Macros are automatically available at crate root when defined with #[macro_export]
