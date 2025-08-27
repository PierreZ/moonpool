//! Thread-local random number generation for simulation.
//!
//! This module provides deterministic randomness through thread-local storage,
//! enabling clean API design without passing RNG through the simulation state.
//! Each thread maintains its own RNG state, ensuring deterministic behavior
//! within each simulation run while supporting parallel test execution.

use rand::SeedableRng;
use rand::{
    Rng,
    distributions::{Distribution, Standard, uniform::SampleUniform},
};
use rand_chacha::ChaCha8Rng;
use std::cell::RefCell;

thread_local! {
    /// Thread-local random number generator for simulation.
    ///
    /// Uses ChaCha8Rng for deterministic, reproducible randomness.
    /// Each thread maintains independent state for parallel test execution.
    static SIM_RNG: RefCell<ChaCha8Rng> = RefCell::new(ChaCha8Rng::from_entropy());

    /// Thread-local storage for the current simulation seed.
    ///
    /// This stores the last seed set via [`set_sim_seed`] to enable
    /// error reporting with seed information.
    static CURRENT_SEED: RefCell<u64> = const { RefCell::new(0) };
}

/// Generate a random value using the thread-local simulation RNG.
///
/// This function provides deterministic randomness based on the seed set
/// via [`set_sim_seed`]. The same seed will always produce the same sequence
/// of random values within a single thread.
///
/// # Type Parameters
///
/// * `T` - The type to generate. Must implement the Standard distribution.
///
/// # Example
///
/// ```rust
/// use moonpool_simulation::rng::{set_sim_seed, sim_random};
///
/// set_sim_seed(42);
/// let value1: f64 = sim_random();
/// let value2: u32 = sim_random();
///
/// // Reset to same seed - will produce identical sequence
/// set_sim_seed(42);
/// let same_value1: f64 = sim_random();
/// let same_value2: u32 = sim_random();
///
/// assert_eq!(value1, same_value1);
/// assert_eq!(value2, same_value2);
/// ```
pub fn sim_random<T>() -> T
where
    Standard: Distribution<T>,
{
    SIM_RNG.with(|rng| rng.borrow_mut().sample(Standard))
}

/// Generate a random value within a specified range using the thread-local simulation RNG.
///
/// This function provides deterministic randomness for values within a range.
/// The same seed will always produce the same sequence of values.
///
/// # Type Parameters
///
/// * `T` - The type to generate. Must implement SampleUniform.
///
/// # Parameters
///
/// * `range` - The range to sample from (exclusive upper bound).
///
/// # Example
///
/// ```rust
/// use moonpool_simulation::rng::{set_sim_seed, sim_random_range};
/// use std::time::Duration;
///
/// set_sim_seed(42);
/// let delay_ms = sim_random_range(100..1000);
/// let delay = Duration::from_millis(delay_ms);
///
/// // delay will be between 100ms and 999ms
/// assert!(delay >= Duration::from_millis(100));
/// assert!(delay < Duration::from_millis(1000));
/// ```
pub fn sim_random_range<T>(range: std::ops::Range<T>) -> T
where
    T: SampleUniform + PartialOrd,
{
    SIM_RNG.with(|rng| rng.borrow_mut().gen_range(range))
}

/// Generate a random value within the given range, returning the start value if the range is empty.
///
/// This is a safe version of [`sim_random_range`] that handles empty ranges gracefully
/// by returning the start value when start == end.
///
/// # Parameters
///
/// * `range` - The range to sample from (start..end)
///
/// # Returns
///
/// A random value within the range, or the start value if the range is empty.
///
/// # Example
///
/// ```rust
/// use moonpool_simulation::{set_sim_seed, sim_random_range_or_default};
///
/// set_sim_seed(42);
/// let value = sim_random_range_or_default(0.0..0.0); // Returns 0.0 (start value)
/// let value2 = sim_random_range_or_default(1.0..5.0); // Returns random value 1.0-5.0
/// ```
pub fn sim_random_range_or_default<T>(range: std::ops::Range<T>) -> T
where
    T: SampleUniform + PartialOrd + Clone,
{
    if range.start >= range.end {
        range.start
    } else {
        sim_random_range(range)
    }
}

/// Set the seed for the thread-local simulation RNG.
///
/// This function initializes the thread-local RNG with a specific seed,
/// ensuring deterministic behavior. The same seed will always produce
/// the same sequence of random values.
///
/// # Parameters
///
/// * `seed` - The seed value to use for deterministic randomness.
///
/// # Example
///
/// ```rust
/// use moonpool_simulation::rng::{set_sim_seed, sim_random};
///
/// // Set a specific seed for reproducible randomness
/// set_sim_seed(12345);
///
/// let first_value: f64 = sim_random();
/// let second_value: f64 = sim_random();
///
/// // Reset to same seed - will reproduce the same sequence
/// set_sim_seed(12345);
/// assert_eq!(first_value, sim_random::<f64>());
/// assert_eq!(second_value, sim_random::<f64>());
/// ```
pub fn set_sim_seed(seed: u64) {
    SIM_RNG.with(|rng| {
        *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(seed);
    });
    CURRENT_SEED.with(|current| {
        *current.borrow_mut() = seed;
    });
}

/// Get the current simulation seed.
///
/// Returns the seed that was last set via [`set_sim_seed`].
/// This is useful for error reporting to help reproduce failing test cases.
///
/// # Returns
///
/// The current simulation seed, or 0 if no seed has been set.
///
/// # Example
///
/// ```rust
/// use moonpool_simulation::rng::{set_sim_seed, get_current_sim_seed};
///
/// set_sim_seed(12345);
/// assert_eq!(get_current_sim_seed(), 12345);
/// ```
pub fn get_current_sim_seed() -> u64 {
    CURRENT_SEED.with(|current| *current.borrow())
}

/// Reset the thread-local simulation RNG to a fresh state.
///
/// This function clears any existing RNG state and initializes with entropy.
/// It should be called before setting a new seed to ensure clean state
/// between consecutive simulation runs on the same thread.
///
/// # Example
///
/// ```rust
/// use moonpool_simulation::rng::{reset_sim_rng, set_sim_seed, sim_random};
///
/// // Run first simulation
/// set_sim_seed(42);
/// let _value1: f64 = sim_random();
///
/// // Clean state before next simulation
/// reset_sim_rng();
/// set_sim_seed(123);
/// let _value2: f64 = sim_random();
/// ```
pub fn reset_sim_rng() {
    SIM_RNG.with(|rng| {
        *rng.borrow_mut() = ChaCha8Rng::from_entropy();
    });
    CURRENT_SEED.with(|current| {
        *current.borrow_mut() = 0;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_randomness() {
        // Set seed and generate some values
        set_sim_seed(42);
        let value1: f64 = sim_random();
        let value2: u32 = sim_random();
        let value3: bool = sim_random();

        // Reset to same seed and verify same sequence
        set_sim_seed(42);
        assert_eq!(value1, sim_random::<f64>());
        assert_eq!(value2, sim_random::<u32>());
        assert_eq!(value3, sim_random::<bool>());
    }

    #[test]
    fn test_different_seeds_produce_different_values() {
        // Generate values with first seed
        set_sim_seed(1);
        let value1_seed1: f64 = sim_random();
        let value2_seed1: f64 = sim_random();

        // Generate values with different seed
        set_sim_seed(2);
        let value1_seed2: f64 = sim_random();
        let value2_seed2: f64 = sim_random();

        // Values should be different
        assert_ne!(value1_seed1, value1_seed2);
        assert_ne!(value2_seed1, value2_seed2);
    }

    #[test]
    fn test_sim_random_range() {
        set_sim_seed(42);

        // Test integer range
        for _ in 0..100 {
            let value = sim_random_range(10..20);
            assert!(value >= 10);
            assert!(value < 20);
        }

        // Test f64 range
        for _ in 0..100 {
            let value = sim_random_range(0.0..1.0);
            assert!(value >= 0.0);
            assert!(value < 1.0);
        }
    }

    #[test]
    fn test_range_determinism() {
        set_sim_seed(123);
        let value1 = sim_random_range(100..1000);
        let value2 = sim_random_range(0.0..10.0);

        set_sim_seed(123);
        assert_eq!(value1, sim_random_range(100..1000));
        assert_eq!(value2, sim_random_range(0.0..10.0));
    }

    #[test]
    fn test_reset_clears_state() {
        // Set seed and advance RNG
        set_sim_seed(42);
        let _advance1: f64 = sim_random();
        let _advance2: f64 = sim_random();
        let after_advance: f64 = sim_random();

        // Reset and set same seed - should get first value, not third
        reset_sim_rng();
        set_sim_seed(42);
        let first_value: f64 = sim_random();

        // Should be different because reset cleared the advanced state
        assert_ne!(after_advance, first_value);
    }

    #[test]
    fn test_sequence_persistence_within_thread() {
        set_sim_seed(42);
        let value1: f64 = sim_random();
        let value2: f64 = sim_random();
        let value3: f64 = sim_random();

        // Values should form a deterministic sequence
        set_sim_seed(42);
        assert_eq!(value1, sim_random::<f64>());
        assert_eq!(value2, sim_random::<f64>());
        assert_eq!(value3, sim_random::<f64>());
    }

    #[test]
    fn test_multiple_resets_and_seeds() {
        // Test multiple reset/seed cycles
        for seed in [1, 42, 12345] {
            reset_sim_rng();
            set_sim_seed(seed);
            let first: f64 = sim_random();

            reset_sim_rng();
            set_sim_seed(seed);
            assert_eq!(first, sim_random::<f64>());
        }
    }

    #[test]
    fn test_get_current_sim_seed() {
        // Test getting current seed after setting
        set_sim_seed(12345);
        assert_eq!(get_current_sim_seed(), 12345);

        set_sim_seed(98765);
        assert_eq!(get_current_sim_seed(), 98765);

        // Test that reset clears the seed
        reset_sim_rng();
        assert_eq!(get_current_sim_seed(), 0);
    }
}
