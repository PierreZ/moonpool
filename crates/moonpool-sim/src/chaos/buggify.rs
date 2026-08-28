//! Simulation wiring for the standalone [`moonpool_buggify`] crate.
//!
//! The buggify state and the [`buggify!`](crate::buggify) /
//! [`buggify_with_prob!`](crate::buggify_with_prob) macros live in
//! `moonpool-buggify` (zero dependencies, usable from production and sans-I/O
//! code). This module installs the simulation's deterministic seeded RNG into
//! that crate for the duration of a run, and keeps the simulation-specific
//! knob randomization ([`buggify_knob!`](crate::buggify_knob)).
//!
//! Because both crates share the one thread-local state in `moonpool-buggify`,
//! macros imported through either crate observe the same activation decisions
//! during a simulation.

use crate::sim::rng::sim_random_f64;
use rand::distr::uniform::SampleUniform;

pub use moonpool_buggify::buggify_internal;

/// Initialize buggify for a simulation run.
///
/// Installs the simulation's seeded RNG as the buggify random source, then
/// enables buggify with the given activation probability. Each buggify
/// location is randomly activated once per run; active locations fire
/// probabilistically on each call.
pub fn buggify_init(activation_prob: f64, firing_prob: f64) {
    moonpool_buggify::set_random_source(sim_random_f64);
    moonpool_buggify::buggify_init(activation_prob, firing_prob);
}

/// Reset/disable buggify and uninstall the simulation random source.
///
/// Buggify is inert again after this call, as it is outside an active
/// simulation.
pub fn buggify_reset() {
    moonpool_buggify::buggify_reset();
    moonpool_buggify::clear_random_source();
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

/// Buggify a config knob *value* within bounds.
///
/// `buggify_knob!(default, lo..hi)` evaluates to `default` on most seeds, but when
/// buggify is enabled and this call site is activated + fires (same model as
/// [`buggify!`](crate::buggify)) it evaluates to a random value in `lo..hi`.
/// Deterministic per `(location, seed)`, so replay is exact. Mirrors
/// `FoundationDB`'s `if (randomize && BUGGIFY) KNOB = random(lo, hi)`.
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

    #[test]
    fn test_macro_paths_share_state() {
        // The sim re-export and the standalone crate must hit the same state.
        set_sim_seed(4242);
        buggify_init(1.0, 1.0);
        assert_eq!(
            moonpool_buggify::buggify_internal(1.0, "shared_state"),
            buggify_internal(1.0, "shared_state"),
        );
        assert!(moonpool_buggify::buggify_internal(1.0, "shared_state"));
        buggify_reset();
        assert!(!moonpool_buggify::buggify_internal(1.0, "shared_state"));
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
            .collect::<std::collections::BTreeSet<_>>()
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
