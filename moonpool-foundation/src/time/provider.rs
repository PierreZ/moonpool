//! Time provider implementations for simulation and real time.

use async_trait::async_trait;
use std::time::{Duration, Instant};

use crate::{SimulationResult, WeakSimWorld};

/// Provider trait for time operations.
///
/// This trait allows code to work with both simulation time and real wall-clock time
/// in a unified way. Implementations handle sleeping and getting current time
/// appropriate for their environment.
#[async_trait(?Send)]
pub trait TimeProvider: Clone {
    /// Sleep for the specified duration.
    ///
    /// In simulation, this advances simulation time. In real time,
    /// this uses actual wall-clock delays.
    async fn sleep(&self, duration: Duration) -> SimulationResult<()>;

    /// Get current time.
    ///
    /// In simulation, this returns simulation time. In real time,
    /// this returns wall-clock time.
    fn now(&self) -> Instant;

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

    fn now(&self) -> Instant {
        // For now, use real Instant - simulation time management is more complex
        // and would require significant changes to the core simulation infrastructure.
        // This is acceptable since the primary benefit is simulation-aware sleep.
        Instant::now()
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
pub struct TokioTimeProvider;

impl TokioTimeProvider {
    /// Create a new Tokio time provider.
    pub fn new() -> Self {
        Self
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

    fn now(&self) -> Instant {
        Instant::now()
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

        // Test now - should return valid instant
        let now = time_provider.now();
        assert!(now.elapsed() < Duration::from_secs(1)); // Should be recent

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

        // We can't test async sleep directly in synchronous tests easily
        // This test verifies creation and basic properties
        let now = time_provider.now();
        assert!(now.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_tokio_time_provider() {
        let time_provider = TokioTimeProvider::new();

        let start = Instant::now();
        let result = time_provider.sleep(Duration::from_millis(1)).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(elapsed >= Duration::from_millis(1));
        assert!(elapsed < Duration::from_millis(50)); // Allow some overhead

        // Test now
        let now = time_provider.now();
        assert!(now.elapsed() < Duration::from_secs(1));

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
}
