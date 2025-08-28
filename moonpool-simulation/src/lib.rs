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
//! ## Example Usage
//!
//! ```rust
//! use moonpool_simulation::{SimWorld, Event};
//! use std::time::Duration;
//!
//! let mut sim = SimWorld::new();
//!
//! // Schedule a wake event
//! sim.schedule_event(Event::Wake { task_id: 1 }, Duration::from_millis(100));
//!
//! // Process events
//! sim.run_until_empty();
//!
//! assert_eq!(sim.current_time(), Duration::from_millis(100));
//! ```

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

/// Assertion macros and result tracking for simulation testing.
pub mod assertions;
/// Error types and utilities for simulation operations.
pub mod error;
/// Event scheduling and processing for the simulation engine.
pub mod events;
/// Network simulation and abstraction layer.
pub mod network;
/// Network state management for simulation.
mod network_state;
/// Thread-local random number generation for simulation.
pub mod rng;
/// Simulation runner and statistical analysis framework.
pub mod runner;
/// Core simulation world and coordination logic.
pub mod sim;
/// Sleep functionality for simulation time.
pub mod sleep;
/// Time provider abstraction for simulation and real time.
pub mod time;

// Public API exports
pub use assertions::{AssertionStats, get_assertion_results, validate_assertion_contracts};
pub use error::{SimulationError, SimulationResult};
pub use events::{Event, EventQueue, ScheduledEvent};
// Network exports
pub use network::{
    CloggingConfiguration, LatencyConfiguration, LatencyRange, NetworkConfiguration,
    NetworkProvider, NetworkRandomizationRanges, Peer, PeerConfig, PeerError, PeerMetrics,
    SimNetworkProvider, TcpListenerTrait, TokioNetworkProvider,
};
// Time provider exports
pub use rng::{
    get_current_sim_seed, reset_sim_rng, set_sim_seed, sim_random, sim_random_range,
    sim_random_range_or_default,
};
pub use runner::{SimulationBuilder, SimulationMetrics, SimulationReport, WorkloadTopology};
pub use sim::{SimWorld, WeakSimWorld};
pub use sleep::SleepFuture;
pub use time::{SimTimeProvider, TimeProvider, TokioTimeProvider};

// Macros are automatically available at crate root when defined with #[macro_export]
