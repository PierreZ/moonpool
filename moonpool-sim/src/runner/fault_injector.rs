//! Fault injector trait for simulation testing.
//!
//! Fault injectors run concurrently with workloads during the run phase
//! and inject faults (network partitions, connection drops, etc.).

use async_trait::async_trait;

use crate::SimulationError;

use super::context::SimContext;

/// A fault injector that runs concurrently with workloads.
///
/// Fault injectors are started alongside workloads during the run phase
/// and cancelled when the chaos duration expires (if `PhaseConfig` is set).
#[async_trait(?Send)]
pub trait FaultInjector {
    /// Human-readable name for this fault injector.
    fn name(&self) -> &str;

    /// Inject faults during the simulation.
    async fn inject(&self, ctx: &SimContext) -> Result<(), SimulationError>;
}

/// Configuration for phased simulation execution.
///
/// When set, the simulation runs in two phases:
/// 1. **Chaos phase** (`chaos_duration_ms`): Workloads + fault injectors run concurrently.
/// 2. **Liveness phase** (`liveness_duration_ms`): Fault injectors are cancelled,
///    workloads continue to verify the system recovers.
#[derive(Debug, Clone)]
pub struct PhaseConfig {
    /// Duration of the chaos phase in milliseconds.
    pub chaos_duration_ms: u64,
    /// Duration of the liveness phase in milliseconds.
    pub liveness_duration_ms: u64,
}
