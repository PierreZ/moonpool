//! Sleep functionality for simulation time.
//!
//! This module provides the ability to sleep in simulation time using async futures
//! that integrate with the event system. The sleep future will complete when its
//! corresponding Wake event is processed by the simulation engine.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::{SimulationResult, WeakSimWorld};

/// Future that completes after a specified simulation time duration.
///
/// This future integrates with the simulation's event system by:
/// 1. Scheduling a Wake event for the specified duration
/// 2. Registering a waker to be called when the event is processed
/// 3. Returning `Poll::Pending` until the wake event fires
///
/// The future will complete with `Ok(())` when the simulation time has advanced
/// to the scheduled wake time.
pub struct SleepFuture {
    /// Weak reference to the simulation world
    sim: WeakSimWorld,
    /// Unique identifier for this sleep task
    task_id: u64,
    /// Whether this future has already completed
    completed: bool,
}

impl SleepFuture {
    /// Creates a new sleep future.
    ///
    /// This is typically called by `SimWorld::sleep()` and should not be
    /// constructed directly by user code.
    pub fn new(sim: WeakSimWorld, task_id: u64) -> Self {
        Self {
            sim,
            task_id,
            completed: false,
        }
    }
}

impl Future for SleepFuture {
    type Output = SimulationResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // If we've already completed, return immediately
        if self.completed {
            return Poll::Ready(Ok(()));
        }

        // Try to get a reference to the simulation
        let sim = match self.sim.upgrade() {
            Ok(sim) => sim,
            Err(e) => return Poll::Ready(Err(e)),
        };

        // Check if our wake event has been processed
        match sim.is_task_awake(self.task_id) {
            Ok(true) => {
                // Task has been awakened, mark as completed and return ready
                self.completed = true;
                Poll::Ready(Ok(()))
            }
            Ok(false) => {
                // Task hasn't been awakened yet, register waker and return pending
                match sim.register_task_waker(self.task_id, cx.waker().clone()) {
                    Ok(()) => Poll::Pending,
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}
