//! Simulation time provider implementation.

use async_trait::async_trait;
use std::time::Duration;

use moonpool_core::{TimeError, TimeProvider};

use crate::sim::WeakSimWorld;

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
    async fn sleep(&self, duration: Duration) -> Result<(), TimeError> {
        let sleep_future = self.sim.sleep(duration).map_err(|_| TimeError::Shutdown)?;
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

    async fn timeout<F, T>(&self, duration: Duration, future: F) -> Result<T, TimeError>
    where
        F: std::future::Future<Output = T>,
    {
        let sleep_future = self.sim.sleep(duration).map_err(|_| TimeError::Shutdown)?;

        // Race the future against the timeout using tokio::select!
        // Both futures respect simulation time through the event system
        tokio::select! {
            result = future => Ok(result),
            _ = sleep_future => Err(TimeError::Elapsed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimWorld;

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
        assert_eq!(result, Ok(42));
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
