//! Thread-local random number generation for simulation.
//!
//! This module provides deterministic randomness through thread-local storage,
//! enabling clean API design without passing RNG through the simulation state.
//! Each thread maintains its own RNG state, ensuring deterministic behavior
//! within each simulation run while supporting parallel test execution.

use rand::SeedableRng;
use rand::{
    RngExt,
    distr::{Distribution, StandardUniform, uniform::SampleUniform},
};
use rand_chacha::ChaCha8Rng;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

thread_local! {
    /// Thread-local random number generator for simulation.
    ///
    /// Uses ChaCha8Rng for deterministic, reproducible randomness.
    /// Each thread maintains independent state for parallel test execution.
    static SIM_RNG: RefCell<ChaCha8Rng> = RefCell::new(ChaCha8Rng::seed_from_u64(0));

    /// Thread-local storage for the current simulation seed.
    ///
    /// This stores the last seed set via [`set_sim_seed`] to enable
    /// error reporting with seed information.
    static CURRENT_SEED: RefCell<u64> = const { RefCell::new(0) };

    /// Thread-local counter tracking RNG calls since last reset.
    ///
    /// Used by the exploration framework to record fork points and
    /// enable deterministic replay via breakpoints.
    static RNG_CALL_COUNT: Cell<u64> = const { Cell::new(0) };

    /// Thread-local queue of RNG breakpoints, sorted by target call count.
    ///
    /// Each entry is `(target_count, new_seed)`. When the call count exceeds
    /// `target_count`, the RNG reseeds with `new_seed` and the count resets to 1.
    static RNG_BREAKPOINTS: RefCell<VecDeque<(u64, u64)>> = const { RefCell::new(VecDeque::new()) };
}

/// Increment the RNG call counter and check for breakpoints.
///
/// Called before every RNG sample. If the current call count exceeds
/// a breakpoint's target, reseeds the RNG and resets the counter.
fn pre_sample() {
    RNG_CALL_COUNT.with(|c| c.set(c.get() + 1));
    check_rng_breakpoint();
}

/// Check and trigger any pending RNG breakpoints.
///
/// Pops breakpoints whose target count has been exceeded (using `>`),
/// reseeding the RNG for each. The count resets to 1 because the
/// current call is the first call of the new seed segment.
fn check_rng_breakpoint() {
    RNG_BREAKPOINTS.with(|bp| {
        let mut breakpoints = bp.borrow_mut();
        while let Some(&(target_count, new_seed)) = breakpoints.front() {
            let count = RNG_CALL_COUNT.with(|c| c.get());
            if count > target_count {
                breakpoints.pop_front();
                SIM_RNG.with(|rng| {
                    *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(new_seed);
                });
                CURRENT_SEED.with(|s| {
                    *s.borrow_mut() = new_seed;
                });
                RNG_CALL_COUNT.with(|c| c.set(1));
            } else {
                break;
            }
        }
    });
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
/// Generate a random value using the thread-local simulation RNG.
pub fn sim_random<T>() -> T
where
    StandardUniform: Distribution<T>,
{
    pre_sample();
    SIM_RNG.with(|rng| rng.borrow_mut().sample(StandardUniform))
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
/// Generate a random value within a specified range.
pub fn sim_random_range<T>(range: std::ops::Range<T>) -> T
where
    T: SampleUniform + PartialOrd,
{
    pre_sample();
    SIM_RNG.with(|rng| rng.borrow_mut().random_range(range))
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
/// Generate a random value in range or return start value if range is empty.
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
/// Set the seed for the thread-local simulation RNG.
pub fn set_sim_seed(seed: u64) {
    SIM_RNG.with(|rng| {
        *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(seed);
    });
    CURRENT_SEED.with(|current| {
        *current.borrow_mut() = seed;
    });
}

/// Generate a random f64 in the range [0.0, 1.0) using the simulation RNG.
///
/// This is a convenience function matching FDB's `deterministicRandom()->random01()`.
///
/// # Returns
///
/// A random f64 value in [0.0, 1.0).
pub fn sim_random_f64() -> f64 {
    pre_sample();
    SIM_RNG.with(|rng| rng.borrow_mut().sample(StandardUniform))
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
/// Get the current simulation seed.
pub fn get_current_sim_seed() -> u64 {
    CURRENT_SEED.with(|current| *current.borrow())
}

/// Reset the thread-local simulation RNG to a fresh state.
///
/// This function clears any existing RNG state and initializes with entropy.
/// It should be called before setting a new seed to ensure clean state
/// between consecutive simulation runs on the same thread.
///
/// Reset the thread-local simulation RNG to a fresh state.
pub fn reset_sim_rng() {
    SIM_RNG.with(|rng| {
        *rng.borrow_mut() = ChaCha8Rng::seed_from_u64(0);
    });
    CURRENT_SEED.with(|current| {
        *current.borrow_mut() = 0;
    });
    RNG_CALL_COUNT.with(|c| c.set(0));
    RNG_BREAKPOINTS.with(|bp| bp.borrow_mut().clear());
}

/// Get the current RNG call count.
///
/// Returns the number of RNG calls made since the last seed set or reset.
/// Used by the exploration framework to record fork points.
pub fn get_rng_call_count() -> u64 {
    RNG_CALL_COUNT.with(|c| c.get())
}

/// Reset the RNG call count to zero.
///
/// Used when reseeding to start a new counting segment.
pub fn reset_rng_call_count() {
    RNG_CALL_COUNT.with(|c| c.set(0));
}

/// Set RNG breakpoints for deterministic replay.
///
/// Each breakpoint is a `(target_count, new_seed)` pair. When the RNG call
/// count exceeds `target_count`, the RNG is reseeded with `new_seed` and
/// the count resets to 1.
///
/// Breakpoints must be sorted by `target_count` in ascending order.
///
/// # Parameters
///
/// * `breakpoints` - Sorted list of (target_count, new_seed) pairs.
pub fn set_rng_breakpoints(breakpoints: Vec<(u64, u64)>) {
    RNG_BREAKPOINTS.with(|bp| {
        *bp.borrow_mut() = VecDeque::from(breakpoints);
    });
}

/// Clear all RNG breakpoints.
pub fn clear_rng_breakpoints() {
    RNG_BREAKPOINTS.with(|bp| bp.borrow_mut().clear());
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

    #[test]
    fn test_call_counting() {
        reset_sim_rng();
        set_sim_seed(42);
        assert_eq!(get_rng_call_count(), 0);

        let _: f64 = sim_random();
        assert_eq!(get_rng_call_count(), 1);

        let _: u32 = sim_random();
        assert_eq!(get_rng_call_count(), 2);

        let _ = sim_random_range(0..100);
        assert_eq!(get_rng_call_count(), 3);

        let _ = sim_random_f64();
        assert_eq!(get_rng_call_count(), 4);

        // sim_random_range_or_default with valid range delegates to sim_random_range
        let _ = sim_random_range_or_default(0..100);
        assert_eq!(get_rng_call_count(), 5);

        // sim_random_range_or_default with empty range does NOT consume RNG
        let _ = sim_random_range_or_default(100..100);
        assert_eq!(get_rng_call_count(), 5);
    }

    #[test]
    fn test_breakpoint_reseed() {
        reset_sim_rng();
        set_sim_seed(100);

        // Record first 5 values with seed 100
        let mut old_values = Vec::new();
        for _ in 0..5 {
            old_values.push(sim_random::<f64>());
        }

        // Record first value with seed 200 from scratch
        reset_sim_rng();
        set_sim_seed(200);
        let new_seed_first: f64 = sim_random();

        // Replay: seed 100, breakpoint at count=5 to reseed to 200
        reset_sim_rng();
        set_sim_seed(100);
        set_rng_breakpoints(vec![(5, 200)]);

        // First 5 calls should match old seed
        for (i, expected) in old_values.iter().enumerate() {
            let actual: f64 = sim_random();
            assert_eq!(*expected, actual, "Mismatch at call {}", i + 1);
        }

        // Call 6 triggers breakpoint (count 6 > 5), reseeds to 200
        let after_breakpoint: f64 = sim_random();
        assert_eq!(after_breakpoint, new_seed_first);
        assert_eq!(get_rng_call_count(), 1);
        assert_eq!(get_current_sim_seed(), 200);
    }

    #[test]
    fn test_chained_breakpoints() {
        reset_sim_rng();
        set_sim_seed(10);
        set_rng_breakpoints(vec![(3, 20), (2, 30)]);

        // 3 calls with seed 10
        let _: f64 = sim_random(); // count=1
        let _: f64 = sim_random(); // count=2
        let _: f64 = sim_random(); // count=3
        assert_eq!(get_current_sim_seed(), 10);

        // Call 4: count becomes 4 > 3, breakpoint fires: reseed to 20, count=1
        let _: f64 = sim_random();
        assert_eq!(get_current_sim_seed(), 20);
        assert_eq!(get_rng_call_count(), 1);

        // 1 more call with seed 20
        let _: f64 = sim_random(); // count=2

        // Call 3 of seed 20: count becomes 3 > 2, breakpoint fires: reseed to 30, count=1
        let _: f64 = sim_random();
        assert_eq!(get_current_sim_seed(), 30);
        assert_eq!(get_rng_call_count(), 1);
    }

    #[test]
    fn test_replay_determinism() {
        // Run 1: record a "recipe" — seed 42, fork at call 3 to seed 99
        reset_sim_rng();
        set_sim_seed(42);
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        let fork_count = get_rng_call_count();
        set_sim_seed(99);
        reset_rng_call_count();
        let post_fork_1: f64 = sim_random();
        let post_fork_2: f64 = sim_random();

        // Run 2: replay using breakpoints
        reset_sim_rng();
        set_sim_seed(42);
        set_rng_breakpoints(vec![(fork_count, 99)]);
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        // Breakpoint triggers on next call (count 4 > 3)
        let replay_1: f64 = sim_random();
        let replay_2: f64 = sim_random();

        assert_eq!(post_fork_1, replay_1);
        assert_eq!(post_fork_2, replay_2);
    }

    #[test]
    fn test_reset_clears_everything_including_breakpoints() {
        set_sim_seed(42);
        let _: f64 = sim_random();
        let _: f64 = sim_random();
        set_rng_breakpoints(vec![(10, 99)]);

        assert_eq!(get_rng_call_count(), 2);

        reset_sim_rng();

        assert_eq!(get_rng_call_count(), 0);
        assert_eq!(get_current_sim_seed(), 0);

        // Verify breakpoints were cleared
        set_sim_seed(42);
        let _: f64 = sim_random();
        assert_eq!(get_rng_call_count(), 1);
        assert_eq!(get_current_sim_seed(), 42); // no breakpoint triggered
    }
}
