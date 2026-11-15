//! Driving a future and the world together, for low-level tests.
//!
//! `SimulationBuilder::run` drives the scheduler and the executor for
//! process and workload tests. A test of one provider has no runner: its
//! future completes only when the world processes the events that the
//! future scheduled. The three methods here are that loop, written once.

use std::{
    future::Future,
    pin::{Pin, pin},
    task::{Context, Poll, Waker},
};

use super::SimWorld;
use crate::executor::Executor;

impl SimWorld {
    /// The most events [`settle`](Self::settle) processes for one future.
    pub const MAX_SETTLE_STEPS: usize = 100_000;

    /// Run `future` to completion, processing one event of this world each
    /// time the future is pending.
    ///
    /// Await it on moonpool's executor ([`Executor::block_on`], or
    /// [`SimWorld::block_on`] for the common case). A future that is pending
    /// while the world has no event left stays pending, and the executor
    /// then reports the stall with its seed.
    pub async fn drive<F: Future>(&mut self, future: F) -> F::Output {
        let mut future = pin!(future);
        std::future::poll_fn(|context| match future.as_mut().poll(context) {
            Poll::Ready(output) => Poll::Ready(output),
            Poll::Pending => {
                if self.has_pending_events() {
                    self.step();
                    context.waker().wake_by_ref();
                }
                Poll::Pending
            }
        })
        .await
    }

    /// Run `future` to completion on a new executor seeded with `seed`:
    /// [`drive`](Self::drive) inside [`Executor::block_on`].
    ///
    /// # Panics
    ///
    /// Panics when the future is pending and neither an event nor a task is
    /// left to make progress (the executor's stall report).
    pub fn block_on<F: Future>(&mut self, seed: u64, future: F) -> F::Output {
        Executor::new(seed).block_on(self.drive(future))
    }

    /// Poll `future` without an executor, processing one event after each
    /// pending poll, until it is ready or the world has no event left.
    ///
    /// `Poll::Pending` means the future is parked on an empty world: a test
    /// asserts that, then changes the world and settles the same pinned
    /// future again. The future must not depend on spawned tasks.
    ///
    /// # Panics
    ///
    /// Panics after [`MAX_SETTLE_STEPS`](Self::MAX_SETTLE_STEPS) events
    /// without an answer.
    pub fn settle<F: Future + ?Sized>(&mut self, mut future: Pin<&mut F>) -> Poll<F::Output> {
        let mut context = Context::from_waker(Waker::noop());
        for _ in 0..Self::MAX_SETTLE_STEPS {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return Poll::Ready(output);
            }
            if !self.has_pending_events() {
                return Poll::Pending;
            }
            self.step();
        }
        panic!(
            "simulation-backed future exceeded {} events",
            Self::MAX_SETTLE_STEPS
        )
    }
}
