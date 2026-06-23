//! Deterministic fault injection following `FoundationDB`'s buggify approach.
//!
//! Each buggify location is randomly activated once per simulation run.
//! Active locations fire probabilistically on each call.

use crate::sim::rng::sim_random;
use rand::distr::uniform::SampleUniform;
use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

#[derive(Default)]
struct State {
    enabled: bool,
    active_locations: BTreeMap<String, bool>,
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
#[must_use]
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

/// Buggify a knob *value* within bounds.
///
/// Returns `default` on most seeds, but when this call site is buggify-activated
/// and fires (same two-phase model as [`buggify_internal`]) it returns a random
/// value drawn from `range`. Deterministic per `(location, seed)`; varies across
/// seeds. Mirrors `FoundationDB`'s `if (randomize && BUGGIFY) KNOB = random(lo, hi)`
/// (`Buggify.h`): a knob keeps its configured value most of the time, but a given
/// seed occasionally spikes it to an extreme within `range`.
#[must_use]
pub fn buggify_knob_internal<T>(default: T, range: std::ops::Range<T>, location: &'static str) -> T
where
    T: SampleUniform + PartialOrd + Clone,
{
    if buggify_internal(0.25, location) {
        crate::sim::sim_random_range_or_default(range)
    } else {
        default
    }
}

/// Buggify with 25% probability
#[macro_export]
macro_rules! buggify {
    () => {
        $crate::chaos::buggify::buggify_internal(0.25, concat!(file!(), ":", line!()))
    };
}

/// Buggify with custom probability
#[macro_export]
macro_rules! buggify_with_prob {
    ($prob:expr) => {
        $crate::chaos::buggify::buggify_internal($prob as f64, concat!(file!(), ":", line!()))
    };
}

/// Buggify a config knob *value* within bounds.
///
/// `buggify_knob!(default, lo..hi)` evaluates to `default` on most seeds, but when
/// buggify is enabled and this call site is activated + fires (same model as
/// [`buggify!`]) it evaluates to a random value in `lo..hi`. Deterministic per
/// `(location, seed)`, so replay is exact. Mirrors `FoundationDB`'s
/// `if (randomize && BUGGIFY) KNOB = random(lo, hi)`.
#[macro_export]
macro_rules! buggify_knob {
    ($default:expr, $range:expr) => {
        $crate::chaos::buggify::buggify_knob_internal(
            $default,
            $range,
            concat!(file!(), ":", line!()),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::rng::{reset_sim_rng, set_sim_seed};

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
                let location = format!("loc_{i}");
                results.push(buggify_internal(0.5, Box::leak(location.into_boxed_str())));
            }

            buggify_reset();
            reset_sim_rng();
        }

        assert_eq!(results1, results2);
    }

    /// Collect a fixed sequence of `buggify_knob!` results for one seed.
    fn knob_sequence(seed: u64) -> Vec<u64> {
        reset_sim_rng();
        set_sim_seed(seed);
        buggify_init(0.8, 0.8);
        let mut out = Vec::new();
        for i in 0..20 {
            // Distinct call-site identity per index without a real source line.
            let location = Box::leak(format!("knob_{i}").into_boxed_str());
            out.push(buggify_knob_internal::<u64>(100, 1_000..2_000, location));
        }
        buggify_reset();
        out
    }

    #[test]
    fn test_buggify_knob_deterministic() {
        const SEED: u64 = 98765;
        assert_eq!(knob_sequence(SEED), knob_sequence(SEED));
    }

    #[test]
    fn test_buggify_knob_varies_across_seeds() {
        let sequences: Vec<Vec<u64>> = [111u64, 222, 333, 444, 555]
            .iter()
            .map(|s| knob_sequence(*s))
            .collect();
        let unique = sequences
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(
            unique > 1,
            "different seeds should yield different knob spikes"
        );
    }

    #[test]
    fn test_buggify_knob_disabled_returns_default() {
        reset_sim_rng();
        set_sim_seed(42);
        buggify_reset(); // disabled: never spike
        for _ in 0..10 {
            assert_eq!(buggify_knob_internal::<u64>(100, 1_000..2_000, "loc"), 100);
        }
    }

    #[test]
    fn test_buggify_knob_spiked_value_in_range() {
        reset_sim_rng();
        set_sim_seed(7);
        buggify_init(1.0, 1.0); // always active + fire
        let v = buggify_knob_internal::<u64>(100, 1_000..2_000, "always");
        buggify_reset();
        assert!(
            (1_000..2_000).contains(&v),
            "spiked value must be in range, got {v}"
        );
    }
}
