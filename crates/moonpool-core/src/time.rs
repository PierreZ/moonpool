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
