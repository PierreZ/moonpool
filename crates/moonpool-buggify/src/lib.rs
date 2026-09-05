//! Standalone buggify fault injection following `FoundationDB`'s approach.
//!
//! Buggify marks code locations where a rare-but-legal behavior can be forced
//! during simulation testing: an early timeout, a dropped buffer, a slow path.
//! Each location is randomly **activated** once per simulation run; active
//! locations then fire probabilistically on each call.
//!
//! This crate is dependency-free and owns only the disabled-by-default state
//! and the [`buggify!`] / [`buggify_with_prob!`] macros, so production and
//! sans-I/O code can depend on it without pulling in a simulation runtime:
//!
//! - Outside an active simulation, buggify is **inert**: every call site
//!   evaluates to `false` with no side effects.
//! - A simulation runtime (such as `moonpool-sim`) enables buggify at the
//!   start of a run via [`buggify_init`] after installing its deterministic
//!   seeded random source via [`set_random_source`], and disables it again
//!   with [`buggify_reset`].
//!
//! State is thread-local, matching the single-threaded deterministic executors
//! that drive moonpool simulations, and shared by every path into this crate —
//! macros invoked through a re-export (e.g. `moonpool_sim::buggify!`) hit the
//! same state as macros invoked through `moonpool_buggify::buggify!`.
//!
//! # Usage
//!
//! ```
//! use moonpool_buggify::buggify;
//!
//! // Inert unless a simulation runtime has enabled buggify on this thread.
//! if buggify!() {
//!     // Simulate a rare failure path.
//! }
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Deterministic random source: returns an `f64` in `[0.0, 1.0)`.
///
/// A plain function pointer so the crate stays dependency-free; simulation
/// runtimes install their seeded generator (e.g. `sim_random_f64`).
pub type RandomSource = fn() -> f64;

#[derive(Default)]
struct State {
    enabled: bool,
    active_locations: BTreeMap<String, bool>,
    activation_prob: f64,
    random_source: Option<RandomSource>,
}

/// Install the deterministic random source used for activation and firing draws.
///
/// Called by the simulation runtime before [`buggify_init`]. Without an
/// installed source, buggify stays inert even if enabled.
pub fn set_random_source(source: RandomSource) {
    STATE.with(|state| {
        state.borrow_mut().random_source = Some(source);
    });
}

/// Remove the installed random source, returning buggify to its inert state.
pub fn clear_random_source() {
    STATE.with(|state| {
        state.borrow_mut().random_source = None;
    });
}

/// Initialize buggify for a simulation run.
///
/// Clears per-location activation decisions from any previous run and enables
/// firing with the given activation probability. The firing probability is
/// not a run-level knob: [`buggify!`] fires an active location at 25% per
/// call, and [`buggify_with_prob!`] takes its own per-site rate. The random
/// source must have been installed via [`set_random_source`] for call sites
/// to fire.
pub fn buggify_init(activation_prob: f64) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.enabled = true;
        state.active_locations.clear();
        state.activation_prob = activation_prob;
    });
}

/// Reset/disable buggify.
///
/// After this call every buggify site evaluates to `false` until the next
/// [`buggify_init`]. The installed random source is left in place; use
/// [`clear_random_source`] to remove it as well.
pub fn buggify_reset() {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.enabled = false;
        state.active_locations.clear();
        state.activation_prob = 0.0;
    });
}

/// Internal buggify implementation backing the [`buggify!`] and
/// [`buggify_with_prob!`] macros.
///
/// Decides activation for `location` on first encounter (one random draw),
/// then fires probabilistically on each call while active (one draw per call).
/// Draw order and count are stable for a given call sequence, so seeded replay
/// through an installed [`RandomSource`] is exact.
#[must_use]
pub fn buggify_internal(prob: f64, location: &'static str) -> bool {
    STATE.with(|state| {
        let mut state = state.borrow_mut();

        if !state.enabled || prob <= 0.0 {
            return false;
        }
        let Some(random) = state.random_source else {
            return false;
        };

        let location_str = location.to_string();
        let activation_prob = state.activation_prob;

        // Decide activation on first encounter
        let is_active = *state
            .active_locations
            .entry(location_str)
            .or_insert_with(|| random() < activation_prob);

        // If active, fire probabilistically
        is_active && random() < prob
    })
}

/// Buggify with 25% probability
#[macro_export]
macro_rules! buggify {
    () => {
        $crate::buggify_internal(0.25, concat!(file!(), ":", line!()))
    };
}

/// Buggify with custom probability
#[macro_export]
macro_rules! buggify_with_prob {
    ($prob:expr) => {
        $crate::buggify_internal($prob as f64, concat!(file!(), ":", line!()))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// Deterministic test source: a simple counter-driven sequence.
        static TEST_DRAWS: Cell<u64> = const { Cell::new(0) };
    }

    /// Test random source: cycles deterministically through [0.0, 1.0).
    fn test_source() -> f64 {
        let n = TEST_DRAWS.with(|c| {
            let n = c.get();
            c.set(n + 1);
            n
        });
        // Multiplicative hash into [0, 1), deterministic per draw index. The
        // shift leaves 32 bits, so the u32 conversion is lossless.
        let bits = u32::try_from(n.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32)
            .expect("shifted value fits in u32");
        f64::from(bits) / 4_294_967_296.0
    }

    fn reset_test_source() {
        TEST_DRAWS.with(|c| c.set(0));
    }

    #[test]
    fn disabled_by_default_is_inert() {
        buggify_reset();
        clear_random_source();
        for _ in 0..10 {
            assert!(!buggify_internal(1.0, "inert"));
        }
    }

    #[test]
    fn enabled_without_source_is_inert() {
        clear_random_source();
        buggify_init(1.0);
        assert!(!buggify_internal(1.0, "no_source"));
        buggify_reset();
    }

    #[test]
    fn activation_decision_is_consistent() {
        reset_test_source();
        set_random_source(test_source);
        buggify_init(0.5);

        let location = "consistent_location";
        let first = buggify_internal(1.0, location);
        let second = buggify_internal(1.0, location);
        // With prob=1.0 the outcome equals the activation decision, which is
        // made once per location.
        assert_eq!(first, second);
        buggify_reset();
        clear_random_source();
    }

    #[test]
    fn sequences_replay_deterministically() {
        let run = || {
            reset_test_source();
            set_random_source(test_source);
            buggify_init(0.5);
            let out: Vec<bool> = (0..5)
                .map(|i| {
                    let location = Box::leak(format!("loc_{i}").into_boxed_str());
                    buggify_internal(0.5, location)
                })
                .collect();
            buggify_reset();
            clear_random_source();
            out
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn always_active_always_fires() {
        reset_test_source();
        set_random_source(test_source);
        buggify_init(1.0);
        let fired = (0..20).any(|_| buggify_internal(1.0, "always"));
        assert!(fired, "activation 1.0 + prob 1.0 must fire");
        buggify_reset();
        clear_random_source();
    }

    #[test]
    fn reset_disables_firing() {
        reset_test_source();
        set_random_source(test_source);
        buggify_init(1.0);
        assert!(buggify_internal(1.0, "reset_case"));
        buggify_reset();
        assert!(!buggify_internal(1.0, "reset_case"));
        clear_random_source();
    }

    #[test]
    fn macros_share_crate_state() {
        reset_test_source();
        set_random_source(test_source);
        buggify_init(1.0);
        // buggify! fires at 25% per call; over many calls it must fire.
        assert!(
            (0..100).any(|_| crate::buggify!()),
            "macro must observe enabled state"
        );
        assert!(crate::buggify_with_prob!(1.0));
        buggify_reset();
        assert!((0..100).all(|_| !crate::buggify!()));
        assert!(!crate::buggify_with_prob!(1.0));
        clear_random_source();
    }
}
