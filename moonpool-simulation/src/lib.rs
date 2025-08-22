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
/// Thread-local random number generation for simulation.
pub mod rng;
/// Simulation runner and statistical analysis framework.
pub mod runner;
/// Core simulation world and coordination logic.
pub mod sim;
/// Sleep functionality for simulation time.
pub mod sleep;

// Public API exports
pub use assertions::AssertionStats;
pub use error::{SimulationError, SimulationResult};
pub use events::{Event, EventQueue, ScheduledEvent};
// Network exports
pub use network::{
    LatencyConfiguration, LatencyRange, NetworkConfiguration, NetworkProvider, SimNetworkProvider,
    TcpListenerTrait, TokioNetworkProvider,
};
pub use rng::{reset_sim_rng, set_sim_seed, sim_random, sim_random_range};
pub use runner::{SimulationBuilder, SimulationMetrics, SimulationReport, WorkloadTopology};
pub use sim::{SimWorld, WeakSimWorld};
pub use sleep::SleepFuture;

// Macros are automatically available at crate root when defined with #[macro_export]
