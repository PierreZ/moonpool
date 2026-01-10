//! Random number generation provider abstraction.
//!
//! This module provides a provider pattern for random number generation,
//! consistent with other provider abstractions in the simulation framework
//! like TimeProvider, NetworkProvider, and TaskProvider.
//!
//! # Providers
//!
//! - [`ThreadRngRandomProvider`]: Production provider using thread-local RNG
//! - `SimRandomProvider` (in moonpool-sim): Deterministic simulation provider

use rand::Rng;
use rand::distr::{Distribution, StandardUniform, uniform::SampleUniform};
use std::ops::Range;

/// Provider trait for random number generation.
///
/// This trait abstracts random number generation to enable both
/// deterministic simulation randomness and real random numbers
/// in a unified way. Implementations handle the source of randomness
/// appropriate for their environment.
pub trait RandomProvider: Clone {
    /// Generate a random value of type T.
    ///
    /// The type T must implement the Standard distribution.
    fn random<T>(&self) -> T
    where
        StandardUniform: Distribution<T>;

    /// Generate a random value within a specified range.
    ///
    /// The range is exclusive of the upper bound (start..end).
    fn random_range<T>(&self, range: Range<T>) -> T
    where
        T: SampleUniform + PartialOrd;

    /// Generate a random f64 between 0.0 and 1.0.
    ///
    /// This is a convenience method for generating ratios and percentages.
    fn random_ratio(&self) -> f64;

    /// Generate a random bool with the given probability of being true.
    ///
    /// The probability should be between 0.0 and 1.0.
    fn random_bool(&self, probability: f64) -> bool;
}

/// Production random provider using thread-local RNG.
///
/// This provider uses `rand::thread_rng()` for random number generation,
/// which is suitable for production use but NOT for deterministic simulation.
///
/// For simulation testing, use `SimRandomProvider` from moonpool-sim instead.
///
/// # Example
///
/// ```
/// use moonpool_core::{RandomProvider, ThreadRngRandomProvider};
///
/// let random = ThreadRngRandomProvider::new();
/// let value: u64 = random.random();
/// let in_range = random.random_range(1..100);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadRngRandomProvider;

impl ThreadRngRandomProvider {
    /// Create a new thread-local RNG provider.
    pub fn new() -> Self {
        Self
    }
}

impl RandomProvider for ThreadRngRandomProvider {
    fn random<T>(&self) -> T
    where
        StandardUniform: Distribution<T>,
    {
        rand::rng().random()
    }

    fn random_range<T>(&self, range: Range<T>) -> T
    where
        T: SampleUniform + PartialOrd,
    {
        rand::rng().random_range(range)
    }

    fn random_ratio(&self) -> f64 {
        rand::rng().random()
    }

    fn random_bool(&self, probability: f64) -> bool {
        rand::rng().random_bool(probability)
    }
}
