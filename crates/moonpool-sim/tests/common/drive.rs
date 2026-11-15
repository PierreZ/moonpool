//! Drive a simulation-backed future by hand, without an executor: a thin
//! wrapper over [`SimWorld::settle`].
//!
//! Shared by the test binaries that include it with `#[path]`.

use std::future::Future;
use std::pin::pin;
use std::task::Poll;

use moonpool_sim::SimWorld;

/// Run `future` to completion; a future parked on an empty world is a
/// test failure.
pub fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    match sim.settle(pin!(future)) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("simulation-backed future stalled without a pending event"),
    }
}
