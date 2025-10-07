//! Deterministic fault injection following FoundationDB's buggify approach.
//!
//! Each buggify location is randomly activated once per simulation run.
//! Active locations fire probabilistically on each call.

use crate::rng::sim_random;
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

#[derive(Default)]
struct State {
    enabled: bool,
    active_locations: HashMap<String, bool>,
    activation_prob: f64,
}

/// Initialize buggify for simulation run
pub fn buggify_init(activation_prob: f64, _firing_prob: f64) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.enabled = true;
        state.active_locations.clear();
        state.activation_prob = activation_prob;
    });
}

/// Reset/disable buggify
pub fn buggify_reset() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.enabled = false;
        state.active_locations.clear();
        state.activation_prob = 0.0;
    });
}

/// Internal buggify implementation
pub fn buggify_internal(prob: f64, location: &'static str) -> bool {
    STATE.with(|state| {
        let mut state = state.borrow_mut();

        if !state.enabled || prob <= 0.0 {
            return false;
        }

        let location_str = location.to_string();
        let activation_prob = state.activation_prob;

        // Decide activation on first encounter
        let is_active = *state
            .active_locations
            .entry(location_str)
            .or_insert_with(|| sim_random::<f64>() < activation_prob);

        // If active, fire probabilistically
        is_active && sim_random::<f64>() < prob
    })
}

/// Buggify with 25% probability
#[macro_export]
macro_rules! buggify {
    () => {
        $crate::buggify::buggify_internal(0.25, concat!(file!(), ":", line!()))
    };
}

/// Buggify with custom probability
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
    fn test_disabled_by_default() {
        buggify_reset();
        for _ in 0..10 {
            assert!(!buggify_internal(1.0, "test"));
        }
    }

    #[test]
    fn test_activation_consistency() {
        set_sim_seed(12345);
        buggify_init(0.5, 1.0);

        let location = "test_location";
        let first = buggify_internal(1.0, location);
        let second = buggify_internal(1.0, location);

        // Activation decision should be consistent
        assert_eq!(first, second);
        buggify_reset();
    }

    #[test]
    fn test_deterministic() {
        const SEED: u64 = 54321;
        let mut results1 = Vec::new();
        let mut results2 = Vec::new();

        for run in 0..2 {
            set_sim_seed(SEED);
            buggify_init(0.5, 0.5);

            let results = if run == 0 {
                &mut results1
            } else {
                &mut results2
            };

            for i in 0..5 {
                let location = format!("loc_{}", i);
                results.push(buggify_internal(0.5, Box::leak(location.into_boxed_str())));
            }

            buggify_reset();
            reset_sim_rng();
        }

        assert_eq!(results1, results2);
    }
}
