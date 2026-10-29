//! The [`Workload`] trait: the test driver of a simulation.
//!
//! A workload drives the system under test and judges it. It is the
//! client, the load generator and the oracle. Unlike a
//! [`Process`](super::process::Process), it is never rebooted: its
//! in-memory state (what it sent, what was acknowledged) survives every
//! fault, so it can check the system against it.
//!
//! Each workload goes through three lifecycle phases:
//!
//! 1. **Setup**: initialize state; workloads set up one after another.
//! 2. **Run**: the main logic; all workloads run concurrently. When the
//!    first one returns, the shutdown token fires for everyone.
//! 3. **Check**: validate the final state after the run and the drain;
//!    workloads check one after another.
//!
//! Prefer one workload that does many things, with
//! [`swarm_operations`](super::builder::SimulationBuilder::swarm_operations)
//! choosing a per-seed subset of its operations, over many narrow workloads.
//!
//! # Usage
//!
//! ```rust
//! use async_trait::async_trait;
//! use moonpool_sim::{SimContext, SimulationResult, Workload, assert_always};
//!
//! #[derive(Default)]
//! struct Counter {
//!     sent: u64,
//! }
//!
//! #[async_trait]
//! impl Workload for Counter {
//!     fn name(&self) -> &str {
//!         "counter"
//!     }
//!
//!     async fn run(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
//!         // Drive the system; remember what was sent.
//!         self.sent += 1;
//!         Ok(())
//!     }
//!
//!     async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
//!         // Judge the final state against what the workload remembers.
//!         assert_always!(self.sent == 1, "the run sent exactly one request");
//!         Ok(())
//!     }
//! }
//! ```

use async_trait::async_trait;

use crate::SimulationResult;

use super::context::{ProviderBundle, SimContext};
use crate::SimProviders;

/// A workload that participates in simulation testing.
///
/// Workloads are the primary unit of distributed system behavior. The simulation
/// framework calls lifecycle methods in order: `setup` → `run` → `check`.
#[async_trait]
pub trait Workload<P: ProviderBundle = SimProviders>: Send + Sync + 'static {
    /// Name of this workload for topology and reporting.
    fn name(&self) -> &str;

    /// Setup phase: bind listeners, initialize state.
    ///
    /// All workloads complete setup before any `run()` starts.
    /// Default implementation is a no-op.
    async fn setup(&mut self, _ctx: &SimContext<P>) -> SimulationResult<()> {
        Ok(())
    }

    /// Run phase: main workload logic.
    ///
    /// All workloads run concurrently. Ends on completion or shutdown signal.
    async fn run(&mut self, ctx: &SimContext<P>) -> SimulationResult<()>;

    /// Check phase: validate correctness after quiescence.
    ///
    /// Called after all runs complete and pending events drain.
    /// Its verdict is part of the iteration's result: an `Err` (or a panic)
    /// fails the seed exactly as a failing `run()` does, even when every
    /// `run()` returned `Ok`.
    /// Default implementation is a no-op.
    async fn check(&mut self, _ctx: &SimContext<P>) -> SimulationResult<()> {
        Ok(())
    }
}
