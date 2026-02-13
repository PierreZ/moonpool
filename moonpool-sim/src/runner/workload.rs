//! Workload trait for simulation testing.
//!
//! The `Workload` trait defines the lifecycle of a simulation workload:
//! setup, run, and check phases.

use async_trait::async_trait;

use crate::SimulationError;

use super::context::SimContext;

/// A simulation workload with setup, run, and check phases.
///
/// # Lifecycle
///
/// 1. **setup** - Initialize state, create connections, register endpoints.
///    Called sequentially for all workloads before the run phase.
/// 2. **run** - Execute the main workload logic concurrently with other workloads
///    and fault injectors.
/// 3. **check** - Validate final state after all workloads complete.
///    Called sequentially after the run phase and `sim.run_until_empty()`.
#[async_trait(?Send)]
pub trait Workload {
    /// Human-readable name for this workload.
    fn name(&self) -> &str;

    /// Setup phase: initialize state before the run phase.
    ///
    /// Default implementation is a no-op.
    async fn setup(&self, _ctx: &SimContext) -> Result<(), SimulationError> {
        Ok(())
    }

    /// Run phase: execute the main workload logic.
    async fn run(&self, ctx: &SimContext) -> Result<(), SimulationError>;

    /// Check phase: validate final state after all workloads complete.
    ///
    /// Default implementation is a no-op.
    async fn check(&self, _ctx: &SimContext) -> Result<(), SimulationError> {
        Ok(())
    }
}

/// A workload adapter that wraps an async closure.
pub struct FnWorkload<F> {
    name: String,
    run_fn: F,
}

/// Create a workload from a name and async closure.
///
/// # Example
///
/// ```ignore
/// let w = workload_fn("my_workload", |ctx| async move {
///     // workload logic
///     Ok(())
/// });
/// ```
pub fn workload_fn<F, Fut>(name: &str, run_fn: F) -> FnWorkload<F>
where
    F: Fn(&SimContext) -> Fut,
    Fut: std::future::Future<Output = Result<(), SimulationError>>,
{
    FnWorkload {
        name: name.to_string(),
        run_fn,
    }
}

#[async_trait(?Send)]
impl<F, Fut> Workload for FnWorkload<F>
where
    F: Fn(&SimContext) -> Fut,
    Fut: std::future::Future<Output = Result<(), SimulationError>>,
{
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, ctx: &SimContext) -> Result<(), SimulationError> {
        (self.run_fn)(ctx).await
    }
}
