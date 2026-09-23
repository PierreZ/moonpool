//! Drive a simulation-backed future by hand, without an executor: poll it
//! with a no-op waker and step the world between polls.
//!
//! Shared by the test binaries that include it with `#[path]`.

use std::future::Future;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

use moonpool_sim::SimWorld;

/// Upper bound on the events one driven future may consume.
pub const MAX_DRIVER_STEPS: usize = 100_000;

/// Poll a simulation-backed future until it resolves or the world runs out of
/// events. `Pending` means it is parked for good: nothing is left to wake it.
pub fn settle<F: Future + ?Sized>(sim: &mut SimWorld, mut future: Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..MAX_DRIVER_STEPS {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Poll::Ready(output);
        }
        if !sim.has_pending_events() {
            return Poll::Pending;
        }
        sim.step();
    }
    panic!("simulation-backed future exceeded {MAX_DRIVER_STEPS} events")
}

/// Drive `future` to completion, panicking if it stalls with no pending
/// event left to wake it.
pub fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    let mut future = pin!(future);
    match settle(sim, future.as_mut()) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("simulation-backed future stalled without a pending event"),
    }
}
