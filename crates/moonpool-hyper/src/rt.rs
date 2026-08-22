//! hyper 1.x runtime adapters over moonpool providers.
//!
//! hyper's HTTP/2 support needs two runtime hooks: an
//! [`Executor`](hyper::rt::Executor) to spawn internal tasks (per-request
//! service futures on the server, stream bookkeeping and keepalive on the
//! client) and a [`Timer`](hyper::rt::Timer) for h2 keepalive ping/pong and
//! timeouts. In production those are `hyper_util`'s `TokioExecutor` and
//! `TokioTimer`; this module provides the same hooks over moonpool's
//! [`TaskProvider`] and [`TimeProvider`], so hyper (and stacks built on it,
//! like tonic) runs identically on the tokio providers and inside the
//! deterministic simulation.
//!
//! Determinism note: hyper reads the clock exclusively through
//! [`Timer::now`](hyper::rt::Timer::now) when a timer is configured, and
//! [`HyperTimer`] answers those reads from the provider clock. The `Instant`
//! values it returns are offsets from an arbitrary per-timer anchor, so only
//! differences between them are meaningful, and those differences are pure
//! provider time, fully deterministic under a simulated clock.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use moonpool_core::{Detach, TaskProvider, TimeProvider};

/// A [`hyper::rt::Executor`] backed by a moonpool [`TaskProvider`].
///
/// Spawns hyper's internal futures as detached provider tasks: onto the
/// tokio runtime under `TokioTaskProvider`, onto the deterministic sim
/// executor under `SimTaskProvider`.
#[derive(Clone, Debug)]
pub struct HyperExecutor<T> {
    tasks: T,
}

// hyper's h2 client connection is generic over its executor and requires
// `E: Unpin` (the connection future holds the executor inline). Deriving that
// from `T: Unpin` would force the bound onto every caller's provider generics,
// so declare it once here.
//
// Sound because `HyperExecutor` has no pinned fields and exposes no pinning
// API: `tasks` is only ever accessed through `&self`, never through a
// `Pin` projection, so nothing can rely on the value staying put in memory.
impl<T> Unpin for HyperExecutor<T> {}

impl<T: TaskProvider> HyperExecutor<T> {
    /// Create an executor that spawns via the given task provider.
    pub fn new(tasks: T) -> Self {
        Self { tasks }
    }
}

impl<T, Fut> hyper::rt::Executor<Fut> for HyperExecutor<T>
where
    T: TaskProvider,
    Fut: Future + Send + 'static,
{
    fn execute(&self, fut: Fut) {
        // Fire-and-forget, matching hyper_util's TokioExecutor: hyper manages
        // the lifetime of its internal futures itself.
        self.tasks
            .spawn_task("hyper", async move {
                let _ = fut.await;
            })
            .detach();
    }
}

/// A [`hyper::rt::Timer`] backed by a moonpool [`TimeProvider`].
///
/// Sleeps are provider sleeps, and [`Timer::now`](hyper::rt::Timer::now) is
/// answered from the provider clock, so hyper's h2 keepalive interval and
/// timeout arithmetic runs entirely on provider time.
#[derive(Clone, Debug)]
pub struct HyperTimer<T> {
    time: T,
    /// Arbitrary anchor mapping the provider's `Duration`-since-creation
    /// clock onto the `Instant` values hyper's `Timer` API requires. The
    /// anchor cancels out of every deadline computation hyper performs
    /// (deadlines come from `Timer::now() + interval` and return through
    /// `sleep_until`), so despite being captured from the wall clock it
    /// never influences behavior: provider time does.
    anchor: Instant,
    /// Provider time at construction, subtracted so `now()` stays near the
    /// anchor instead of drifting `provider.now()` past it twice.
    epoch: Duration,
}

impl<T: TimeProvider> HyperTimer<T> {
    /// Create a timer that sleeps and reads the clock via the given provider.
    pub fn new(time: T) -> Self {
        let epoch = time.now();
        Self {
            time,
            anchor: Instant::now(),
            epoch,
        }
    }
}

impl<T: TimeProvider> hyper::rt::Timer for HyperTimer<T> {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn hyper::rt::Sleep>> {
        // Clone the provider into an owned future: the trait's sleep future
        // borrows &self, but hyper needs a 'static Sleep.
        let time = self.time.clone();
        Box::pin(HyperSleep {
            inner: Box::pin(async move {
                // A sleep that errors (provider shutdown) resolves rather
                // than pending forever: hyper treats it as an elapsed timer
                // and unwinds the connection, which is the correct behavior
                // at sim shutdown.
                let _ = time.sleep(duration).await;
            }),
            elapsed: false,
        })
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn hyper::rt::Sleep>> {
        self.sleep(deadline.saturating_duration_since(self.now()))
    }

    fn now(&self) -> Instant {
        self.anchor + self.time.now().saturating_sub(self.epoch)
    }
}

/// Future returned by [`HyperTimer`]'s sleep methods.
struct HyperSleep {
    inner: Pin<Box<dyn Future<Output = ()> + Send + Sync>>,

    /// Latches once the sleep has elapsed, so a later poll answers `Ready`
    /// instead of resuming a finished future.
    elapsed: bool,
}

impl Future for HyperSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // hyper polls a Sleep again after it has already elapsed. h2's
        // keepalive does it on two paths in `maybe_ping`: when the connection
        // is idle and `keep_alive_while_idle` is off, and when a frame arrived
        // while the timer was scheduled. Both leave the finished sleep in
        // place and poll it on the next pass. `tokio::time::Sleep` answers
        // `Ready` there, so hyper relies on that; an async block instead
        // panics with "async fn resumed after completion", which used to take
        // down the connection task and poison hyper's ping mutex.
        if self.elapsed {
            return Poll::Ready(());
        }
        let poll = self.inner.as_mut().poll(cx);
        if poll.is_ready() {
            self.elapsed = true;
        }
        poll
    }
}

impl hyper::rt::Sleep for HyperSleep {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::task::noop_waker_ref;
    use hyper::rt::Timer as _;
    use moonpool_core::{TimeError, TimeProvider};

    use super::{Context, Future, HyperTimer};

    /// A provider whose sleeps are already over, so a single poll elapses the
    /// timer and the second poll is the one under test.
    #[derive(Clone, Debug)]
    struct ElapsedTime;

    impl TimeProvider for ElapsedTime {
        fn sleep(
            &self,
            _duration: Duration,
        ) -> impl Future<Output = Result<(), TimeError>> + Send + Sync {
            std::future::ready(Ok(()))
        }

        fn now(&self) -> Duration {
            Duration::ZERO
        }

        fn timer(&self) -> Duration {
            Duration::ZERO
        }

        async fn timeout<F, T>(&self, _duration: Duration, future: F) -> Result<T, TimeError>
        where
            F: Future<Output = T> + Send,
            T: Send,
        {
            Ok(future.await)
        }
    }

    /// hyper's h2 keepalive polls a Sleep it has already seen elapse, on the
    /// paths in `maybe_ping` where it decides not to send a ping. Answering
    /// `Ready` is what `tokio::time::Sleep` does and what hyper depends on;
    /// resuming the finished future instead panics and takes the connection
    /// with it.
    #[test]
    fn a_sleep_polled_after_it_elapsed_stays_ready() {
        let timer = HyperTimer::new(ElapsedTime);
        let mut sleep = timer.sleep(Duration::from_secs(3));
        let mut cx = Context::from_waker(noop_waker_ref());

        assert!(sleep.as_mut().poll(&mut cx).is_ready());
        assert!(
            sleep.as_mut().poll(&mut cx).is_ready(),
            "a second poll must not resume the finished future"
        );
        assert!(sleep.as_mut().poll(&mut cx).is_ready());
    }
}
