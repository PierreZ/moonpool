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
pub mod context;
pub mod display;
pub mod fault_injector;
pub(crate) mod orchestrator;
pub mod report;
pub mod tokio;
pub mod topology;
pub mod workload;

// Re-export main types at module level
pub use builder::{ClientId, WorkloadCount};
pub use builder::{IterationControl, SimulationBuilder};
pub use context::SimContext;
pub use fault_injector::{FaultContext, FaultInjector, PhaseConfig};
pub use report::{SimulationMetrics, SimulationReport};
pub use tokio::{TokioReport, TokioRunner};
pub use topology::WorkloadTopology;
pub use workload::Workload;
