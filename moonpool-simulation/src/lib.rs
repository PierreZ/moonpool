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

/// Error types and utilities for simulation operations.
pub mod error;
/// Event scheduling and processing for the simulation engine.
pub mod events;
/// Core simulation world and coordination logic.
pub mod sim;

// Public API exports
pub use error::{SimulationError, SimulationResult};
pub use events::{Event, EventQueue, ScheduledEvent};
pub use sim::{SimWorld, WeakSimWorld};
