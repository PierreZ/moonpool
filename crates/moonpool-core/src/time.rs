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

    /// [`timeout`](Self::timeout) for futures that are not `Send`.
    ///
    /// Use it inside a task spawned with
    /// [`LocalTaskProvider::spawn_local`](crate::LocalTaskProvider::spawn_local),
    /// where the awaited future may hold `Rc` or `RefCell` state. The returned
    /// future is not `Send` either.
    ///
    /// Provided in terms of [`sleep`](Self::sleep): the future is polled first,
    /// then the sleep, the same order as `timeout` and as tokio's `Timeout`, so
    /// a future that is ready on the poll where the deadline passes still wins.
    /// Returns `Ok(result)` if the future completes within the timeout,
    /// `Err(TimeError::Elapsed)` if the sleep finishes first, or the sleep's own
    /// error.
    fn timeout_local<F>(
        &self,
        duration: Duration,
        future: F,
    ) -> impl std::future::Future<Output = Result<F::Output, TimeError>>
    where
        F: std::future::Future,
    {
        let sleep = self.sleep(duration);
        async move {
            futures::pin_mut!(future);
            futures::pin_mut!(sleep);
            futures::future::poll_fn(|cx| {
                if let std::task::Poll::Ready(output) = future.as_mut().poll(cx) {
                    return std::task::Poll::Ready(Ok(output));
                }
                match sleep.as_mut().poll(cx) {
                    std::task::Poll::Ready(Ok(())) => {
                        std::task::Poll::Ready(Err(TimeError::Elapsed))
                    }
                    std::task::Poll::Ready(Err(error)) => std::task::Poll::Ready(Err(error)),
                    std::task::Poll::Pending => std::task::Poll::Pending,
                }
            })
            .await
        }
    }
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

#[cfg(all(test, feature = "tokio-time"))]
mod tests {
    use std::{rc::Rc, time::Duration};

    use super::{TimeError, TimeProvider, TokioTimeProvider};

    #[tokio::test]
    async fn timeout_local_completes_a_non_send_future() {
        let time = TokioTimeProvider::new();
        let shared = Rc::new(7_u32);
        let result = time
            .timeout_local(Duration::from_secs(5), async move {
                tokio::task::yield_now().await;
                *shared
            })
            .await;
        assert_eq!(result, Ok(7));
    }

    #[tokio::test]
    async fn timeout_local_elapses_on_a_pending_non_send_future() {
        let time = TokioTimeProvider::new();
        let shared = Rc::new(7_u32);
        let result = time
            .timeout_local(Duration::from_millis(20), async move {
                // `shared` is read after the await, so the future holds
                // the `Rc` across it and is not `Send`.
                std::future::pending::<u32>().await + *shared
            })
            .await;
        assert_eq!(result, Err(TimeError::Elapsed));
    }
}
