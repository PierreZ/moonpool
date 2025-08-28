//! Buggify system for deterministic chaos testing.
//!
//! This module provides the buggify system that enables controlled fault injection
//! during simulation testing, following the FoundationDB approach to chaos engineering.
//!
//! ## Key Principles
//!
//! - **Deterministic**: Same seed produces identical fault patterns
//! - **Single-fire**: Each buggify point fires at most once per test run
//! - **Simulation-only**: No impact on production code
//! - **Simple API**: Just `buggify!()` and `buggify_with_prob!(0.3)`
//!
//! ## Usage
//!
//! ```rust
//! use moonpool_simulation::{buggify, buggify_with_prob};
//!
//! // Default 25% activation probability
//! if buggify!() {
//!     // Force difficult/dangerous path
//!     return early_timeout();
//! }
//!
//! // Custom probability
//! if buggify_with_prob!(0.3) {
//!     // 30% chance to trigger this fault
//!     inject_network_delay().await;
//! }
//! ```

use crate::rng::sim_random;
use std::cell::RefCell;
use std::collections::HashSet;

thread_local! {
    /// Thread-local buggify state for the current simulation run
    static BUGGIFY_STATE: RefCell<BuggifyState> = RefCell::new(BuggifyState::default());
}

/// Internal state for the buggify system
#[derive(Default)]
struct BuggifyState {
    /// Whether buggify is enabled for this simulation run
    enabled: bool,
    /// Set of locations that have already fired in this run (single-fire behavior)
    fired_locations: HashSet<String>,
    /// Probability that a buggify point is active for this run (determined once per run)
    activation_prob: f64,
    /// Probability that an active buggify point fires when reached (default 25%)
    firing_prob: f64,
}

/// Initialize the buggify system for a simulation run
///
/// This should be called once at the start of each simulation test run
/// with the desired probabilities.
pub fn buggify_init(activation_prob: f64, firing_prob: f64) {
    BUGGIFY_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.enabled = true;
        state.fired_locations.clear();
        state.activation_prob = activation_prob;
        state.firing_prob = firing_prob;
    });
}

/// Reset/disable the buggify system
///
/// This should be called after simulation testing to ensure buggify
/// doesn't affect non-simulation code.
pub fn buggify_reset() {
    BUGGIFY_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.enabled = false;
        state.fired_locations.clear();
        state.activation_prob = 0.0;
        state.firing_prob = 0.0;
    });
}

/// Internal implementation for buggify macros
///
/// This function implements the FoundationDB-style buggify behavior:
/// 1. Check if buggify is enabled (simulation only)
/// 2. Check if this location should be active (determined once per run)
/// 3. Check if this location has already fired (single-fire behavior)
/// 4. Probabilistically decide whether to fire
pub fn buggify_internal(prob: f64, location: &'static str) -> bool {
    BUGGIFY_STATE.with(|state| {
        let mut state = state.borrow_mut();

        // If buggify is not enabled, never fire
        if !state.enabled {
            return false;
        }

        let location_str = location.to_string();

        // Check if this location has already fired (single-fire behavior)
        if state.fired_locations.contains(&location_str) {
            return false;
        }

        // Determine if this location should be active for this run
        // This is done deterministically based on the location string
        let should_be_active = {
            // Use a simple hash of the location to determine activation
            // This ensures the same location has the same activation status across runs with same seed
            let location_hash = location_str
                .chars()
                .map(|c| c as u32)
                .fold(0u32, |acc, c| acc.wrapping_mul(31).wrapping_add(c));

            // Convert hash to probability and compare with activation threshold
            let hash_prob = (location_hash % 1000) as f64 / 1000.0;
            hash_prob < state.activation_prob
        };

        if !should_be_active {
            return false;
        }

        // For active locations, probabilistically decide whether to fire
        let should_fire = prob > 0.0 && {
            let random_value: f64 = sim_random();
            random_value < prob
        };

        if should_fire {
            // Mark this location as fired (single-fire behavior)
            state.fired_locations.insert(location_str);
            return true;
        }

        false
    })
}

/// Macro for buggify with default probability (25%)
///
/// This macro provides the primary buggify interface. Use it to inject
/// faults at strategic locations in high-level code (workloads, actors).
///
/// ```rust
/// if buggify!() {
///     // Force the difficult/dangerous path
///     return early_timeout();
/// }
/// ```
#[macro_export]
macro_rules! buggify {
    () => {
        $crate::buggify::buggify_internal(0.25, concat!(file!(), ":", line!()))
    };
}

/// Macro for buggify with custom probability
///
/// This macro allows specifying a custom probability for the buggify point.
/// Use higher probabilities for more critical fault injection points.
///
/// ```rust
/// if buggify_with_prob!(0.4) {
///     // 40% chance to inject this specific fault
///     inject_connection_failure().await;
/// }
/// ```
#[macro_export]
macro_rules! buggify_with_prob {
    ($prob:expr) => {
        $crate::buggify::buggify_internal($prob as f64, concat!(file!(), ":", line!()))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::{reset_sim_rng, set_sim_seed};

    #[test]
    fn test_buggify_disabled_by_default() {
        buggify_reset();

        // Buggify should never fire when disabled
        for _ in 0..100 {
            assert!(!buggify_internal(1.0, "test_location"));
        }
    }

    #[test]
    fn test_buggify_single_fire_behavior() {
        set_sim_seed(12345);
        buggify_init(1.0, 1.0); // 100% activation, 100% firing

        // First call might fire
        let _first_result = buggify_internal(1.0, "test_location");

        // Subsequent calls to same location should never fire
        for _ in 0..10 {
            let subsequent_result = buggify_internal(1.0, "test_location");
            assert!(
                !subsequent_result,
                "Buggify should only fire once per location"
            );
        }

        buggify_reset();
    }

    #[test]
    fn test_buggify_deterministic_behavior() {
        const TEST_SEED: u64 = 54321;

        // Run the same test twice with the same seed
        let mut results1 = Vec::new();
        let mut results2 = Vec::new();

        for run in 0..2 {
            set_sim_seed(TEST_SEED);
            buggify_init(0.5, 0.5);

            let results = if run == 0 {
                &mut results1
            } else {
                &mut results2
            };

            // Test multiple different locations
            for i in 0..10 {
                let location = format!("test_location_{}", i);
                let result = buggify_internal(0.5, Box::leak(location.into_boxed_str()));
                results.push(result);
            }

            buggify_reset();
            reset_sim_rng();
        }

        // Results should be identical for same seed
        assert_eq!(
            results1, results2,
            "Buggify should be deterministic with same seed"
        );
    }

    #[test]
    fn test_buggify_probability_respected() {
        set_sim_seed(11111);
        buggify_init(1.0, 1.0); // All locations active

        // With probability 0.0, should never fire
        for i in 0..50 {
            let location = format!("never_fire_{}", i);
            let result = buggify_internal(0.0, Box::leak(location.into_boxed_str()));
            assert!(!result, "Probability 0.0 should never fire");
        }

        buggify_reset();
    }

    #[test]
    fn test_buggify_activation_probability() {
        set_sim_seed(99999);

        // With 0% activation probability, no locations should be active
        buggify_init(0.0, 1.0);

        for i in 0..20 {
            let location = format!("inactive_location_{}", i);
            let result = buggify_internal(1.0, Box::leak(location.into_boxed_str()));
            assert!(
                !result,
                "0% activation probability should make all locations inactive"
            );
        }

        buggify_reset();
    }
}
