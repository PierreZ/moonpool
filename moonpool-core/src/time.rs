//! Time provider abstraction for simulation and real time.
//!
//! This module provides a unified interface for time operations that works
//! seamlessly with both simulation time and real wall-clock time.

use async_trait::async_trait;
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

/// Provider trait for time operations.
///
/// This trait allows code to work with both simulation time and real wall-clock time
/// in a unified way. Implementations handle sleeping and getting current time
/// appropriate for their environment.
///
/// ## Time Semantics
///
/// - `now()`: Returns the exact, canonical time. Use this for scheduling and precise comparisons.
/// - `timer()`: Returns a potentially drifted time that can be slightly ahead of `now()`.
///   In simulation, this simulates real-world clock drift. In production, this equals `now()`.
///
/// FDB ref: sim2.actor.cpp:1056-1064
#[async_trait(?Send)]
pub trait TimeProvider: Clone {
    /// Sleep for the specified duration.
    ///
    /// In simulation, this advances simulation time. In real time,
    /// this uses actual wall-clock delays.
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError>;

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
    fn timer(&self) -> Duration;

    /// Run a future with a timeout.
    ///
    /// Returns `Ok(result)` if the future completes within the timeout,
    /// or `Err(TimeError::Elapsed)` if it times out.
    async fn timeout<F, T>(&self, duration: Duration, future: F) -> Result<T, TimeError>
    where
        F: std::future::Future<Output = T>;
}

/// Real time provider using Tokio's time facilities.
#[derive(Debug, Clone)]
pub struct TokioTimeProvider {
    /// Start time for calculating elapsed duration
    start_time: std::time::Instant,
}

impl TokioTimeProvider {
    /// Create a new Tokio time provider.
    pub fn new() -> Self {
        Self {
            start_time: std::time::Instant::now(),
        }
    }
}

impl Default for TokioTimeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl TimeProvider for TokioTimeProvider {
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError> {
        tokio::time::sleep(duration).await;
        Ok(())
    }

    fn now(&self) -> Duration {
        // Return elapsed time since provider creation
        self.start_time.elapsed()
    }

    fn timer(&self) -> Duration {
        // In real time, there's no simulated drift - timer equals now
        self.now()
    }

    async fn timeout<F, T>(&self, duration: Duration, future: F) -> Result<T, TimeError>
    where
        F: std::future::Future<Output = T>,
    {
        match tokio::time::timeout(duration, future).await {
            Ok(result) => Ok(result),
            Err(_) => Err(TimeError::Elapsed),
        }
    }
}
