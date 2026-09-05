//! Simulation random provider implementation.

use moonpool_core::RandomProvider;
use rand::distr::{Distribution, StandardUniform, uniform::SampleUniform};
use std::ops::Range;

use crate::sim::rng::{sim_random, sim_random_range};

/// Random provider for simulation that uses the thread-local deterministic RNG.
///
/// This provider wraps the existing thread-local RNG infrastructure in
/// `crate::sim::rng` to provide deterministic random number generation within
/// the simulation environment.
///
/// The provider holds no state and never seeds anything: every draw goes to
/// the thread-local stream that [`set_sim_seed`](crate::set_sim_seed) seeded
/// once for the current simulation (the runner does that per iteration, and
/// [`SimWorld`](crate::SimWorld) construction does it for hand-driven tests).
/// Constructing a provider — at boot, at a mid-run restart, for a workload or
/// a fault injector — therefore never rewinds the simulation's randomness.
#[derive(Clone, Debug, Default)]
pub struct SimRandomProvider {
    // No internal state - uses thread-local RNG from crate::sim::rng
    _marker: std::marker::PhantomData<()>,
}

impl SimRandomProvider {
    /// Create a simulation random provider over the thread-local RNG.
    ///
    /// The stream it draws from must already have been seeded with
    /// [`set_sim_seed`](crate::set_sim_seed); construction does not seed it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RandomProvider for SimRandomProvider {
    fn random<T>(&self) -> T
    where
        StandardUniform: Distribution<T>,
    {
        sim_random()
    }

    fn random_range<T>(&self, range: Range<T>) -> T
    where
        T: SampleUniform + PartialOrd,
    {
        sim_random_range(range)
    }

    fn random_ratio(&self) -> f64 {
        sim_random::<f64>()
    }

    fn random_bool(&self, probability: f64) -> bool {
        debug_assert!(
            (0.0..=1.0).contains(&probability),
            "Probability must be between 0.0 and 1.0, got {probability}"
        );
        sim_random::<f64>() < probability
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::rng::set_sim_seed;

    #[test]
    fn test_deterministic_randomness() {
        // The same seed yields the same values; the provider itself neither
        // seeds nor rewinds the stream, so the test seeds explicitly.
        set_sim_seed(42);
        let provider1 = SimRandomProvider::new();
        let value1_1: f64 = provider1.random();
        let value1_2: u32 = provider1.random();

        set_sim_seed(42);
        let provider2 = SimRandomProvider::new();
        let value2_1: f64 = provider2.random();
        let value2_2: u32 = provider2.random();

        assert_eq!(value1_1.to_bits(), value2_1.to_bits());
        assert_eq!(value1_2, value2_2);
    }

    #[test]
    fn constructing_a_provider_does_not_rewind_the_stream() {
        set_sim_seed(42);
        let first: u64 = SimRandomProvider::new().random();
        let second: u64 = SimRandomProvider::new().random();

        set_sim_seed(42);
        let replay_first: u64 = SimRandomProvider::new().random();
        let replay_second: u64 = SimRandomProvider::new().random();

        assert_eq!(first, replay_first);
        assert_eq!(second, replay_second);
        assert_ne!(first, second, "a fresh provider must continue the stream");
    }

    #[test]
    fn test_random_range() {
        set_sim_seed(123);
        let provider = SimRandomProvider::new();

        // Test integer range
        for _ in 0..100 {
            let value = provider.random_range(10..20);
            assert!(value >= 10);
            assert!(value < 20);
        }

        // Test f64 range
        for _ in 0..100 {
            let value = provider.random_range(0.0..1.0);
            assert!(value >= 0.0);
            assert!(value < 1.0);
        }
    }

    #[test]
    fn test_random_ratio() {
        set_sim_seed(456);
        let provider = SimRandomProvider::new();

        for _ in 0..100 {
            let ratio = provider.random_ratio();
            assert!(ratio >= 0.0);
            assert!(ratio < 1.0);
        }
    }

    #[test]
    fn test_random_bool() {
        set_sim_seed(789);
        let provider = SimRandomProvider::new();

        // Test probability 0.0 - should always be false
        for _ in 0..10 {
            assert!(!provider.random_bool(0.0));
        }

        // Test probability 1.0 - should always be true
        for _ in 0..10 {
            assert!(provider.random_bool(1.0));
        }

        // Test probability 0.5 - should have some variance
        let results: Vec<bool> = (0..100).map(|_| provider.random_bool(0.5)).collect();
        let true_count = results.iter().filter(|&&x| x).count();

        // With 100 samples and 50% probability, we should get roughly 40-60 true values
        // This is a statistical test so it could occasionally fail due to randomness
        assert!(
            true_count > 30 && true_count < 70,
            "Got {true_count} true values out of 100"
        );
    }

    #[test]
    #[should_panic(expected = "Probability must be between 0.0 and 1.0")]
    fn test_random_bool_invalid_probability() {
        let provider = SimRandomProvider::new();
        provider.random_bool(1.5); // Should panic
    }
}
