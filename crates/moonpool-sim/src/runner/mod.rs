//! Simulation runner and orchestration framework.
//!
//! This module provides the infrastructure for running simulation experiments,
//! collecting metrics, and generating comprehensive reports.
//!
//! ## Submodules
//!
//! - `builder` - SimulationBuilder for configuring experiments
//! - `report` - SimulationMetrics and SimulationReport types
//! - `app_metrics` - per-node application metrics scraped from a user registry
//! - `topology` - WorkloadTopology and workload configuration
//! - `orchestrator` - Internal workload orchestration

pub mod app_metrics;
pub mod builder;
mod config;
pub mod context;
pub mod display;
pub mod fault_injector;
pub(crate) mod iteration;
pub mod locality;
pub(crate) mod metrics;
pub(crate) mod orchestrator;
pub mod process;
pub(crate) mod process_manager;
pub mod report;
pub(crate) mod stall;
pub mod tags;
pub mod topology;
pub(crate) mod wall_clock;
pub mod workload;

// Re-export main types at module level
pub use app_metrics::{INSTANCE_LABEL, MetricsHandle};
pub use builder::{Chaos, ChaosMode, ClientId, WorkloadCount};
pub use builder::{IterationControl, SimulationBuilder};
pub use context::SimContext;
pub use fault_injector::{FaultContext, FaultInjector};
pub use locality::{LocalityConfig, MachineRegistry};
pub use process::{Attrition, AttritionScope, Process, RebootKind};
pub use report::{SimulationMetrics, SimulationReport};
pub use tags::{ProcessTags, TagRegistry};
pub use topology::WorkloadTopology;
pub use workload::Workload;
