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
//! Implement the trait directly or use [`workload_fn`] for simple cases:
//!
//! ```ignore
//! use moonpool_sim::{Workload, SimContext, SimulationResult, workload_fn};
//!
//! // Trait implementation
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
//!
//! // Closure shorthand
//! let w = workload_fn("client", |ctx| async move {
//!     Ok(())
//! });
//! ```

use std::future::Future;
use std::pin::Pin;

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

/// Type-erased async closure for workload run function.
type BoxedRunFn =
    Box<dyn FnOnce(&SimContext) -> Pin<Box<dyn Future<Output = SimulationResult<()>>>>>;

/// Closure-based workload adapter.
struct FnWorkload {
    name: String,
    run_fn: Option<BoxedRunFn>,
}

#[async_trait(?Send)]
impl Workload for FnWorkload {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        match self.run_fn.take() {
            Some(f) => f(ctx).await,
            None => Ok(()),
        }
    }
}

/// Create a workload from an async closure.
///
/// The closure receives a `&SimContext` and returns a `SimulationResult<()>`.
/// Only `run()` is implemented; `setup()` and `check()` are no-ops.
///
/// # Example
///
/// ```ignore
/// let w = workload_fn("ping", |ctx| async move {
///     let peer = ctx.peer("server").expect("server");
///     ctx.time().sleep(Duration::from_millis(100)).await?;
///     Ok(())
/// });
/// ```
pub fn workload_fn<F, Fut>(name: impl Into<String>, f: F) -> Box<dyn Workload>
where
    F: FnOnce(&SimContext) -> Fut + 'static,
    Fut: Future<Output = SimulationResult<()>> + 'static,
{
    let name = name.into();
    Box::new(FnWorkload {
        name,
        run_fn: Some(Box::new(move |ctx| Box::pin(f(ctx)))),
    })
}
