//! Simulation runner and orchestration framework.
//!
//! This module provides the infrastructure for running simulation experiments,
//! collecting metrics, and generating comprehensive reports.
//!
//! ## Submodules
//!
//! - `builder` - SimulationBuilder for configuring experiments
//! - `report` - SimulationMetrics and SimulationReport types
//! - `topology` - WorkloadTopology and workload configuration
//! - `orchestrator` - Internal workload orchestration
//! - `tokio` - TokioRunner for real-world execution

pub mod builder;
pub(crate) mod orchestrator;
pub mod report;
pub mod tokio;
pub mod topology;

// Re-export main types at module level
pub use builder::{IterationControl, SimulationBuilder};
pub use report::{SimulationMetrics, SimulationReport};
pub use tokio::{TokioReport, TokioRunner};
pub use topology::WorkloadTopology;
