//! Time provider implementations for simulation and real time.

use async_trait::async_trait;
use std::time::Duration;

use crate::{SimulationResult, WeakSimWorld};

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
    async fn sleep(&self, duration: Duration) -> SimulationResult<()>;

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
    /// or `Err(())` if it times out.
    async fn timeout<F, T>(&self, duration: Duration, future: F) -> SimulationResult<Result<T, ()>>
    where
        F: std::future::Future<Output = T>;
}

/// Simulation time provider that integrates with SimWorld.
#[derive(Debug, Clone)]
pub struct SimTimeProvider {
    sim: WeakSimWorld,
}

impl SimTimeProvider {
    /// Create a new simulation time provider.
    pub fn new(sim: WeakSimWorld) -> Self {
        Self { sim }
    }
}

#[async_trait(?Send)]
impl TimeProvider for SimTimeProvider {
    async fn sleep(&self, duration: Duration) -> SimulationResult<()> {
        let sleep_future = self.sim.sleep(duration)?;
        let _ = sleep_future.await;
        Ok(())
    }

    fn now(&self) -> Duration {
        // Return exact simulation time (equivalent to FDB's now())
        self.sim.now().unwrap_or(Duration::ZERO)
    }

    fn timer(&self) -> Duration {
        // Return drifted timer time (equivalent to FDB's timer())
        // Can be up to clock_drift_max ahead of now()
        self.sim.timer().unwrap_or(Duration::ZERO)
    }

    async fn timeout<F, T>(&self, duration: Duration, future: F) -> SimulationResult<Result<T, ()>>
    where
        F: std::future::Future<Output = T>,
    {
        let sleep_future = self.sim.sleep(duration)?;

        // Race the future against the timeout using tokio::select!
        // Both futures respect simulation time through the event system
        tokio::select! {
            result = future => Ok(Ok(result)),
            _ = sleep_future => Ok(Err(())),
        }
    }
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
    async fn sleep(&self, duration: Duration) -> SimulationResult<()> {
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

    async fn timeout<F, T>(&self, duration: Duration, future: F) -> SimulationResult<Result<T, ()>>
    where
        F: std::future::Future<Output = T>,
    {
        match tokio::time::timeout(duration, future).await {
            Ok(result) => Ok(Ok(result)),
            Err(_) => Ok(Err(())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SimWorld;
    use std::time::Duration;

    #[tokio::test]
    async fn test_sim_time_provider_basic() {
        let sim = SimWorld::new();
        let time_provider = SimTimeProvider::new(sim.downgrade());

        // Test now - should return simulation time (starts at zero)
        let now = time_provider.now();
        assert_eq!(now, Duration::ZERO);

        // Test timer - should be >= now
        let timer = time_provider.timer();
        assert!(timer >= now);

        // Test timeout - quick completion (no simulation advancement needed)
        let result = time_provider
            .timeout(Duration::from_millis(100), async { 42 })
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Ok(42));
    }

    #[test]
    fn test_sim_time_provider_with_simulation() {
        let sim = SimWorld::new();
        let time_provider = SimTimeProvider::new(sim.downgrade());

        // Test now - should return simulation time (starts at zero)
        let now = time_provider.now();
        assert_eq!(now, Duration::ZERO);

        // Test timer - should be >= now but <= now + clock_drift_max
        let timer = time_provider.timer();
        assert!(timer >= now);
        // Default clock_drift_max is 100ms
        assert!(timer <= now + Duration::from_millis(100));
    }

    #[tokio::test]
    async fn test_tokio_time_provider() {
        let time_provider = TokioTimeProvider::new();

        let start = std::time::Instant::now();
        let result = time_provider.sleep(Duration::from_millis(1)).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(elapsed >= Duration::from_millis(1));
        assert!(elapsed < Duration::from_millis(50)); // Allow some overhead

        // Test now - returns elapsed Duration since creation
        let now = time_provider.now();
        assert!(now >= Duration::from_millis(1)); // Should have advanced

        // Test timer - in real time, should be close to now() (may differ slightly due to elapsed time)
        let timer = time_provider.timer();
        let now2 = time_provider.now();
        // Timer should be between the two now() calls or very close
        assert!(timer >= now.saturating_sub(Duration::from_millis(1)));
        assert!(timer <= now2.saturating_add(Duration::from_millis(1)));

        // Test timeout - quick completion
        let result = time_provider
            .timeout(Duration::from_millis(100), async { 42 })
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Ok(42));

        // Test timeout - timeout case
        let result = time_provider
            .timeout(
                Duration::from_millis(1),
                tokio::time::sleep(Duration::from_millis(10)),
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Err(()));
    }

    #[test]
    fn test_time_provider_clone() {
        let tokio_provider = TokioTimeProvider::new();
        let _cloned = tokio_provider.clone();

        let sim = SimWorld::new();
        let sim_provider = SimTimeProvider::new(sim.downgrade());
        let _cloned = sim_provider.clone();
    }

    #[test]
    fn test_clock_drift_bounds() {
        let sim = SimWorld::new();
        let time_provider = SimTimeProvider::new(sim.downgrade());

        // Call timer multiple times to exercise drift logic
        for _ in 0..100 {
            let now = time_provider.now();
            let timer = time_provider.timer();

            // timer should always be >= now
            assert!(timer >= now, "timer ({:?}) < now ({:?})", timer, now);

            // timer should not exceed now + clock_drift_max (100ms default)
            assert!(
                timer <= now + Duration::from_millis(100),
                "timer ({:?}) > now + 100ms ({:?})",
                timer,
                now + Duration::from_millis(100)
            );
        }
    }
}
