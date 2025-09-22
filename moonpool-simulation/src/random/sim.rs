//! Simulation random provider using thread-local deterministic RNG.

use super::RandomProvider;
use crate::rng::{set_sim_seed, sim_random, sim_random_range};
use rand::distributions::{Distribution, Standard, uniform::SampleUniform};
use std::ops::Range;

/// Random provider for simulation that uses the thread-local deterministic RNG.
///
/// This provider wraps the existing thread-local RNG infrastructure in
/// `crate::rng` to provide deterministic random number generation within
/// the simulation environment.
///
/// The provider sets the thread-local seed during construction and then
/// delegates all random generation to the existing `sim_random()` functions.
#[derive(Clone, Debug)]
pub struct SimRandomProvider {
    // No internal state - uses thread-local RNG from crate::rng
    _marker: std::marker::PhantomData<()>,
}

impl SimRandomProvider {
    /// Create a new simulation random provider with the specified seed.
    ///
    /// This sets the thread-local RNG seed using `set_sim_seed()` and
    /// creates a provider that will use that seeded RNG for all operations.
    ///
    /// # Arguments
    ///
    /// * `seed` - The seed value for deterministic random generation
    ///
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::random::sim::SimRandomProvider;
    /// use moonpool_simulation::random::RandomProvider;
    ///
    /// let provider = SimRandomProvider::new(42);
    /// let value: f64 = provider.random();
    /// ```
    pub fn new(seed: u64) -> Self {
        // Set the thread-local RNG seed
        set_sim_seed(seed);

        Self {
            _marker: std::marker::PhantomData,
        }
    }
}

impl RandomProvider for SimRandomProvider {
    fn random<T>(&self) -> T
    where
        Standard: Distribution<T>,
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
            "Probability must be between 0.0 and 1.0, got {}",
            probability
        );
        sim_random::<f64>() < probability
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_randomness() {
        // Two providers with same seed should produce same values
        let provider1 = SimRandomProvider::new(42);
        let value1_1: f64 = provider1.random();
        let value1_2: u32 = provider1.random();

        let provider2 = SimRandomProvider::new(42);
        let value2_1: f64 = provider2.random();
        let value2_2: u32 = provider2.random();

        assert_eq!(value1_1, value2_1);
        assert_eq!(value1_2, value2_2);
    }

    #[test]
    fn test_random_range() {
        let provider = SimRandomProvider::new(123);

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
        let provider = SimRandomProvider::new(456);

        for _ in 0..100 {
            let ratio = provider.random_ratio();
            assert!(ratio >= 0.0);
            assert!(ratio < 1.0);
        }
    }

    #[test]
    fn test_random_bool() {
        let provider = SimRandomProvider::new(789);

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
            "Got {} true values out of 100",
            true_count
        );
    }

    #[test]
    #[should_panic(expected = "Probability must be between 0.0 and 1.0")]
    fn test_random_bool_invalid_probability() {
        let provider = SimRandomProvider::new(999);
        provider.random_bool(1.5); // Should panic
    }
}
