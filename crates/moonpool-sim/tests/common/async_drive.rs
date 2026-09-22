//! Drive a simulation-backed future from inside an async test: step the
//! world whenever the future is pending and an event is queued.
//!
//! Shared by the test binaries that include it with `#[path]`.

use std::future::Future;
use std::task::Poll;

use futures::future::poll_fn;
use moonpool_sim::SimWorld;

/// Poll `future`, stepping `sim` between polls while it has events.
pub async fn drive<F: Future>(sim: &mut SimWorld, future: F) -> F::Output {
    futures::pin_mut!(future);
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(output) => Poll::Ready(output),
        Poll::Pending => {
            if sim.has_pending_events() {
                sim.step();
                cx.waker().wake_by_ref();
            }
            Poll::Pending
        }
    })
    .await
}
