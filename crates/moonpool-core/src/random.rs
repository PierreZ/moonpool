//! Randomness: [`RandomProvider`], with [`TokioRandomProvider`] for
//! production.
//!
//! Every random choice in application code (jitter, a peer to try, a
//! request ID) goes through this trait. In simulation each draw comes from
//! the run's one seeded stream, the same stream the scheduler and the fault
//! injectors use, so one seed replays every choice. A draw from
//! `rand::rng()` or the OS breaks that replay.

#[cfg(feature = "tokio-random")]
use rand::RngExt as _;
use rand::distr::{Distribution, StandardUniform, uniform::SampleUniform};
use std::ops::Range;

/// Provider trait for random number generation.
///
/// Production uses [`TokioRandomProvider`] (a thread-local generator).
/// Simulation uses `moonpool_sim::SimRandomProvider`, which draws from the
/// seeded simulation stream.
pub trait RandomProvider: Clone + Send + Sync + 'static {
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
    fn random_ratio(&self) -> f64 {
        self.random()
    }

    /// Generate a random bool with the given probability of being true.
    ///
    /// The probability should be between 0.0 and 1.0.
    fn random_bool(&self, probability: f64) -> bool {
        self.random_ratio() < probability
    }
}

/// Production random provider using thread-local RNG.
///
/// Uses `rand::rng()` (thread-local, non-cryptographic) for efficient
/// random number generation in production environments.
///
/// # Example
///
/// ```rust
/// use moonpool_core::{RandomProvider, TokioRandomProvider};
///
/// let random = TokioRandomProvider::new();
/// let value: u64 = random.random();
/// let in_range = random.random_range(1..100);
/// ```
#[cfg(feature = "tokio-random")]
#[derive(Clone, Default)]
pub struct TokioRandomProvider;

#[cfg(feature = "tokio-random")]
impl TokioRandomProvider {
    /// Create a new production random provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "tokio-random")]
impl RandomProvider for TokioRandomProvider {
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
}
