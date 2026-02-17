//! Workload trait for simulation testing.
//!
//! Workloads define the distributed system behavior to test. Each workload
//! goes through three lifecycle phases:
//!
//! 1. **Setup**: Bind listeners, initialize state (sequential)
//! 2. **Run**: Main logic, all workloads run concurrently
//! 3. **Check**: Validate correctness after quiescence (sequential)
//!
//! # Usage
//!
//! ```ignore
//! use moonpool_sim::{Workload, SimContext, SimulationResult};
//!
//! struct MyServer;
//!
//! #[async_trait(?Send)]
//! impl Workload for MyServer {
//!     fn name(&self) -> &str { "server" }
//!     async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
//!         // server logic...
//!         Ok(())
//!     }
//! }
//! ```

use async_trait::async_trait;

use crate::SimulationResult;

use super::context::SimContext;

/// A workload that participates in simulation testing.
///
/// Workloads are the primary unit of distributed system behavior. The simulation
/// framework calls lifecycle methods in order: `setup` → `run` → `check`.
#[async_trait(?Send)]
pub trait Workload: 'static {
    /// Name of this workload for topology and reporting.
    fn name(&self) -> &str;

    /// Setup phase: bind listeners, initialize state.
    ///
    /// All workloads complete setup before any `run()` starts.
    /// Default implementation is a no-op.
    async fn setup(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    /// Run phase: main workload logic.
    ///
    /// All workloads run concurrently. Ends on completion or shutdown signal.
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;

    /// Check phase: validate correctness after quiescence.
    ///
    /// Called after all runs complete and pending events drain.
    /// Default implementation is a no-op.
    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }
}
