//! Time: [`TimeProvider`], with [`TokioTimeProvider`] for production.
//!
//! Every sleep, timeout and clock read in application code goes through this
//! trait. In production it is the wall clock. In simulation it is logical
//! time: the simulator jumps the clock to the next event, so a test that
//! sleeps for an hour of simulated time finishes in microseconds. The
//! simulated [`timer`](TimeProvider::timer) also drifts ahead of
//! [`now`](TimeProvider::now), so leases and heartbeats meet clock drift.
//!
//! ```rust
//! use std::time::Duration;
//!
//! use moonpool_core::{TimeError, TimeProvider};
//!
//! /// Wait for `ready`, but never longer than one second.
//! async fn bounded<T: TimeProvider>(time: &T, ready: impl Future<Output = ()> + Send) -> bool {
//!     match time.timeout(Duration::from_secs(1), ready).await {
//!         Ok(()) => true,
//!         Err(TimeError::Elapsed) => false,
//!         Err(TimeError::Shutdown) => false,
//!     }
//! }
//! ```

use std::time::Duration;
use thiserror::Error;

/// Errors that can occur during time operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TimeError {
    /// The operation timed out.
    #[error("operation timed out")]
    Elapsed,

    /// The time provider has been shut down and is no longer accessible.
    #[error("time provider shut down")]
    Shutdown,
}

/// Provider trait for time operations: sleep, timeout and two clocks.
///
/// Production uses [`TokioTimeProvider`]. Simulation uses
/// `moonpool_sim::SimTimeProvider`, which runs on the simulator's logical
/// clock.
///
/// ## Time Semantics
///
/// - `now()`: Returns the exact, canonical time. Use this for scheduling and precise comparisons.
/// - `timer()`: Returns a potentially drifted time that can be slightly ahead of `now()`.
///   In simulation, this simulates real-world clock drift. In production, this equals `now()`.
///
/// FDB ref: sim2.actor.cpp:1056-1064
pub trait TimeProvider: Clone + Send + Sync + 'static {
    /// Sleep for the specified duration.
    ///
    /// In simulation, this advances simulation time. In real time,
    /// this uses actual wall-clock delays.
    fn sleep(
        &self,
        duration: Duration,
    ) -> impl std::future::Future<Output = Result<(), TimeError>> + Send;

    /// Get exact current time.
    ///
    /// In simulation, this returns the canonical simulation time.
    /// In real time, this returns wall-clock elapsed time since provider creation.
    ///
    /// Use this for precise time comparisons and event scheduling.
    fn now(&self) -> Duration;

    /// Get drifted timer time.
    ///
    /// In simulation, this can be up to `clock_drift_max` (default 100ms) ahead of `now()`.
    /// This simulates real-world clock drift between processes.
    /// In real time, this equals `now()`.
    ///
    /// Use this for time-sensitive application logic like timeouts, leases, and heartbeats
    /// to test how code handles clock drift.
    ///
    /// FDB ref: sim2.actor.cpp:1058-1064
    fn timer(&self) -> Duration {
        self.now()
    }

    /// Run a future with a timeout.
    ///
    /// Returns `Ok(result)` if the future completes within the timeout,
    /// or `Err(TimeError::Elapsed)` if it times out.
    fn timeout<F, T>(
        &self,
        duration: Duration,
        future: F,
    ) -> impl std::future::Future<Output = Result<T, TimeError>> + Send
    where
        F: std::future::Future<Output = T> + Send,
        T: Send;
}

/// Local (non-`Send`) form of [`TimeProvider`], for thread-per-core
/// runtimes whose timers cannot leave their thread.
///
/// The same contract as [`TimeProvider`] without any `Send` or `Sync` bound:
/// [`sleep`](Self::sleep) returns a future that need not be `Send`, and
/// [`timeout`](Self::timeout) accepts a future that need not be `Send`. Every
/// [`TimeProvider`] implements it through a blanket impl, so the Tokio and
/// simulated providers get it for free; a thread-per-core backend implements
/// only this trait.
///
/// Code generic over this trait runs on every backend, and when the backend
/// is `Send` (Tokio, the simulator) and the timed future is `Send`, the
/// futures it builds are `Send` too: the auto traits of the concrete future
/// leak through the generic code.
///
/// A concrete [`TimeProvider`] has both traits' methods under the same names.
/// With both traits in scope, `time.sleep(..)` is ambiguous: import one trait
/// per module, or call `LocalTimeProvider::sleep(&time, ..)`.
pub trait LocalTimeProvider: Clone + 'static {
    /// Sleep for the specified duration. See [`TimeProvider::sleep`].
    fn sleep(&self, duration: Duration)
    -> impl std::future::Future<Output = Result<(), TimeError>>;

    /// Get exact current time: the scheduling clock. See
    /// [`TimeProvider::now`].
    fn now(&self) -> Duration;

    /// Get drifted timer time: the application clock. See
    /// [`TimeProvider::timer`].
    fn timer(&self) -> Duration {
        LocalTimeProvider::now(self)
    }

    /// Run a future with a timeout. See [`TimeProvider::timeout`].
    ///
    /// Returns `Ok(result)` if the future completes within the timeout,
    /// or `Err(TimeError::Elapsed)` if it times out.
    fn timeout<F, T>(
        &self,
        duration: Duration,
        future: F,
    ) -> impl std::future::Future<Output = Result<T, TimeError>>
    where
        F: std::future::Future<Output = T>;
}

/// The blanket impl cannot forward [`timeout`](LocalTimeProvider::timeout) to
/// [`TimeProvider::timeout`], which needs a `Send` future. It races the
/// future against [`TimeProvider::sleep`] instead, with the same outcome as
/// `tokio::time::timeout` and the simulator's own `timeout`: the timer starts
/// before the future is first polled, and a future that is ready wins over a
/// timer that is ready in the same poll.
impl<P: TimeProvider> LocalTimeProvider for P {
    fn sleep(
        &self,
        duration: Duration,
    ) -> impl std::future::Future<Output = Result<(), TimeError>> {
        TimeProvider::sleep(self, duration)
    }

    fn now(&self) -> Duration {
        TimeProvider::now(self)
    }

    fn timer(&self) -> Duration {
        TimeProvider::timer(self)
    }

    fn timeout<F, T>(
        &self,
        duration: Duration,
        future: F,
    ) -> impl std::future::Future<Output = Result<T, TimeError>>
    where
        F: std::future::Future<Output = T>,
    {
        race_timeout(future, TimeProvider::sleep(self, duration))
    }
}

/// Race `future` against `sleep`: `Ok` with the future's output, or the
/// sleep's outcome as an error.
///
/// The first poll starts the sleep before it polls the future, so the timer
/// is armed (and, in simulation, scheduled) before any of the future's work;
/// a sleep that fails to start fails the race at once. Every later poll
/// polls the future first. In each poll a ready future wins.
async fn race_timeout<F, S, T>(future: F, sleep: S) -> Result<T, TimeError>
where
    F: std::future::Future<Output = T>,
    S: std::future::Future<Output = Result<(), TimeError>>,
{
    use std::task::Poll;

    let mut future = std::pin::pin!(future);
    let mut sleep = std::pin::pin!(sleep);
    let mut started = false;
    let mut slept = false;
    std::future::poll_fn(|cx| {
        let first_poll = !std::mem::replace(&mut started, true);
        // Arm the timer before any of the future's work.
        if first_poll && let Err(error) = poll_sleep(sleep.as_mut(), cx, &mut slept) {
            return Poll::Ready(Err(error));
        }
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }
        if !first_poll
            && !slept
            && let Err(error) = poll_sleep(sleep.as_mut(), cx, &mut slept)
        {
            return Poll::Ready(Err(error));
        }
        if slept {
            Poll::Ready(Err(TimeError::Elapsed))
        } else {
            Poll::Pending
        }
    })
    .await
}

/// Poll the timeout's sleep once: set `slept` when it fired, and return the
/// error when it failed.
fn poll_sleep<S>(
    sleep: std::pin::Pin<&mut S>,
    cx: &mut std::task::Context<'_>,
    slept: &mut bool,
) -> Result<(), TimeError>
where
    S: std::future::Future<Output = Result<(), TimeError>>,
{
    match sleep.poll(cx) {
        std::task::Poll::Ready(Ok(())) => *slept = true,
        std::task::Poll::Ready(Err(error)) => return Err(error),
        std::task::Poll::Pending => {}
    }
    Ok(())
}

/// Production time provider: `tokio::time` for sleeps and timeouts, and a
/// monotonic [`Instant`](std::time::Instant) for the clocks.
///
/// [`now`](TimeProvider::now) counts from the creation of the provider, and
/// [`timer`](TimeProvider::timer) equals it. Sleeps and timeouts need a
/// running Tokio runtime with the time driver enabled.
#[cfg(feature = "tokio-time")]
#[derive(Debug, Clone)]
pub struct TokioTimeProvider {
    /// Start time for calculating elapsed duration
    start_time: std::time::Instant,
}

#[cfg(feature = "tokio-time")]
impl TokioTimeProvider {
    /// Create a Tokio time provider whose clock starts at zero now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            start_time: std::time::Instant::now(),
        }
    }
}

#[cfg(feature = "tokio-time")]
impl Default for TokioTimeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "tokio-time")]
impl TimeProvider for TokioTimeProvider {
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError> {
        tokio::time::sleep(duration).await;
        Ok(())
    }

    fn now(&self) -> Duration {
        // Return elapsed time since provider creation
        self.start_time.elapsed()
    }

    async fn timeout<F, T>(&self, duration: Duration, future: F) -> Result<T, TimeError>
    where
        F: std::future::Future<Output = T> + Send,
        T: Send,
    {
        tokio::time::timeout(duration, future)
            .await
            .map_err(|_| TimeError::Elapsed)
    }
}
