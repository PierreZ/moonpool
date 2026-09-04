//! Chaos testing module.
//!
//! Contains tests for chaos injection and fault tolerance.

use futures::future::poll_fn;
use moonpool_sim::SimWorld;
use std::future::Future;

async fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    futures::pin_mut!(future);
    poll_fn(|cx| match future.as_mut().poll(cx) {
        std::task::Poll::Ready(output) => std::task::Poll::Ready(output),
        std::task::Poll::Pending => {
            if sim.has_pending_events() {
                sim.step();
                cx.waker().wake_by_ref();
            }
            std::task::Poll::Pending
        }
    })
    .await
}

#[path = "chaos/bit_flip.rs"]
mod bit_flip;
#[path = "chaos/black_hole.rs"]
mod black_hole;
#[path = "chaos/buggified_delay.rs"]
mod buggified_delay;
#[path = "chaos/buggify.rs"]
mod buggify;
#[path = "chaos/clock_drift.rs"]
mod clock_drift;
#[path = "chaos/connect_failure.rs"]
mod connect_failure;
#[path = "chaos/partial_read.rs"]
mod partial_read;
#[path = "chaos/random_close.rs"]
mod random_close;
#[path = "chaos/swarm.rs"]
mod swarm;
