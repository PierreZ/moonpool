//! Random number generation provider abstraction.
//!
//! This module provides a provider pattern for random number generation,
//! consistent with other provider abstractions in the simulation framework
//! like TimeProvider, NetworkProvider, and TaskProvider.

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
