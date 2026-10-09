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

#![deny(missing_docs)]

pub mod hint;

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
    /// The run entered its recovery tail: fault sites stay silent.
    recovering: bool,
    active_locations: BTreeMap<&'static str, bool>,
    activation_prob: f64,
    random_source: Option<RandomSource>,
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|state| f(&mut state.borrow_mut()))
}

/// Whether this thread runs a simulation: buggify is enabled and has a
/// random source (`FoundationDB`'s `g_network->isSimulated()`).
///
/// Code under test may use it to tilt rates, cadences and extra checks in
/// simulation. It should never change an outcome a caller can observe.
#[must_use]
pub fn is_simulated() -> bool {
    with_state(|state| state.enabled && state.random_source.is_some())
}

/// Install the deterministic random source used for activation and firing draws.
///
/// Called by the simulation runtime before [`buggify_init`]. Without an
/// installed source, buggify stays inert even if enabled.
pub fn set_random_source(source: RandomSource) {
    with_state(|state| state.random_source = Some(source));
}

/// Remove the installed random source, returning buggify to its inert state.
pub fn clear_random_source() {
    with_state(|state| state.random_source = None);
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
    with_state(|state| {
        state.enabled = true;
        state.recovering = false;
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
    with_state(|state| {
        state.enabled = false;
        state.recovering = false;
        state.active_locations.clear();
        state.activation_prob = 0.0;
    });
}

/// Enter the run's recovery tail: from now until the next
/// [`buggify_init`], every [`buggify_fault_with_prob!`] site evaluates to
/// `false` without drawing, while [`buggify!`] and [`buggify_with_prob!`]
/// sites keep firing.
///
/// A simulation runtime calls this when its chaos window closes, so the
/// system under test gets a quiet tail to recover in (`FoundationDB` gates
/// its disruptive sites on `speedUpSimulation` for the same reason).
pub fn buggify_enter_recovery() {
    with_state(|state| state.recovering = true);
}

/// Whether [`buggify_enter_recovery`] ran since the last [`buggify_init`].
#[must_use]
pub fn buggify_is_recovering() -> bool {
    with_state(|state| state.recovering)
}

/// Internal implementation backing [`buggify_fault_with_prob!`]: silent,
/// with no draw, once the run is recovering; otherwise
/// [`buggify_internal`].
#[must_use]
pub fn buggify_fault_internal(prob: f64, location: &'static str) -> bool {
    if buggify_is_recovering() {
        return false;
    }
    buggify_internal(prob, location)
}

/// Decide the named location `label` ([`buggify_named!`]) for the current
/// run: `active` replaces its own activation draw.
///
/// A harness calls it when a per-seed scenario needs that location on (or
/// off) together with its other ingredients, so the scenario does not hang
/// on the product of independent activation coins. Call it before the
/// location's first encounter, or at any time to override the decision; it
/// draws nothing. [`buggify_init`] clears it with every other decision.
pub fn set_activation(label: &'static str, active: bool) {
    with_state(|state| {
        state.active_locations.insert(label, active);
    });
}

/// Internal implementation backing [`buggify_named!`]: a disruptive site
/// ([`buggify_fault_internal`]) whose location is its `label`, so a harness
/// can name it in [`set_activation`].
#[must_use]
pub fn buggify_named_internal(prob: f64, label: &'static str) -> bool {
    buggify_fault_internal(prob, label)
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
    with_state(|state| {
        if !state.enabled || prob <= 0.0 {
            return false;
        }
        let Some(random) = state.random_source else {
            return false;
        };
        let activation_prob = state.activation_prob;

        // Decide activation on first encounter
        let is_active = *state
            .active_locations
            .entry(location)
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

/// Buggify a **disruptive** site with a custom probability: one that makes
/// an operation fail (a cut session, a refused request) rather than merely
/// take a rare path.
///
/// Same two-phase model as [`buggify_with_prob!`] during the chaos window;
/// silent once the simulation entered its recovery tail
/// ([`buggify_enter_recovery`]), so liveness checks made after the chaos
/// window are not failed by injection that should have stopped with it.
#[macro_export]
macro_rules! buggify_fault_with_prob {
    ($prob:expr) => {
        $crate::buggify_fault_internal($prob as f64, concat!(file!(), ":", line!()))
    };
}

/// A **disruptive** site keyed by a stable `label` (a `&'static str`)
/// instead of its `file:line`, so a harness can decide its activation per
/// seed with [`set_activation`]. Otherwise it is [`buggify_fault_with_prob!`]:
/// activated once per run on first encounter, fired at `prob` per call
/// while active, silent in the recovery tail.
///
/// Use it for a per-seed decision a scenario couples with others: with
/// `prob` 1.0 the location's activation is the per-seed draw. Every label
/// must be unique in the program: two sites with one label share one
/// activation.
#[macro_export]
macro_rules! buggify_named {
    ($label:expr, $prob:expr) => {
        $crate::buggify_named_internal($prob as f64, $label)
    };
}

/// Report an interesting moment; see [`hint::at`]. Await the result.
///
/// `hint!("label")` fires at [`hint::POINT_PROB`] while active;
/// `hint!("label", 0.2)` at a site rate.
#[macro_export]
macro_rules! hint {
    ($label:literal) => {
        $crate::hint::at(
            $label,
            $crate::hint::POINT_PROB,
            concat!(file!(), ":", line!()),
        )
    };
    ($label:literal, $prob:expr) => {
        $crate::hint::at($label, $prob as f64, concat!(file!(), ":", line!()))
    };
}

/// `Some(i)` with `i` drawn in `0..n` when this site is active and fires,
/// `None` otherwise (and always outside a simulation). For a site that
/// overrides a choice: a column, a row, a target.
#[macro_export]
macro_rules! buggify_pick {
    ($prob:expr, $n:expr) => {
        $crate::buggify_pick_internal($prob as f64, $n, concat!(file!(), ":", line!()))
    };
}

/// `Some(v)` with `v` drawn in `range` (a `Range<u64>`) when this site is
/// active and fires, `None` otherwise (and always outside a simulation).
/// For a site that stretches a value: a delay, a count.
#[macro_export]
macro_rules! buggify_range {
    ($prob:expr, $range:expr) => {
        $crate::buggify_range_internal($prob as f64, $range, concat!(file!(), ":", line!()))
    };
}

/// Internal implementation backing [`buggify_pick!`]: one draw more than
/// [`buggify_internal`] when it fires, none for `n == 0`. `n` is capped at
/// `u32::MAX`.
#[must_use]
pub fn buggify_pick_internal(prob: f64, n: usize, location: &'static str) -> Option<usize> {
    let n = u32::try_from(n).unwrap_or(u32::MAX);
    if n == 0 || !buggify_internal(prob, location) {
        return None;
    }
    let draw = with_state(|state| state.random_source.map_or(0.0, |random| random()));
    usize::try_from(scale(draw, n)).ok()
}

/// The index `i` in `0..n` whose share `[i/n, (i+1)/n)` holds `draw`, a
/// value in `[0, 1)`: a binary search, so no float-to-integer cast.
fn scale(draw: f64, n: u32) -> u32 {
    let (mut low, mut high) = (0, n - 1);
    while low < high {
        let mid = low + (high - low) / 2;
        if draw < f64::from(mid + 1) / f64::from(n) {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    low
}

/// Internal implementation backing [`buggify_range!`]: one draw more than
/// [`buggify_internal`] when it fires, none for an empty range.
#[must_use]
pub fn buggify_range_internal(
    prob: f64,
    range: std::ops::Range<u64>,
    location: &'static str,
) -> Option<u64> {
    if range.is_empty() {
        return None;
    }
    let span = usize::try_from(range.end - range.start).unwrap_or(usize::MAX);
    buggify_pick_internal(prob, span, location)
        .and_then(|offset| u64::try_from(offset).ok())
        .map(|offset| range.start + offset)
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

    /// Run `f` with the test source installed and buggify enabled at
    /// `activation_prob`, then disable buggify and remove the source.
    fn with_test_source<R>(activation_prob: f64, f: impl FnOnce() -> R) -> R {
        TEST_DRAWS.with(|c| c.set(0));
        set_random_source(test_source);
        buggify_init(activation_prob);
        let out = f();
        buggify_reset();
        clear_random_source();
        out
    }

    #[test]
    fn simulated_only_while_enabled_with_a_source() {
        buggify_reset();
        clear_random_source();
        assert!(!is_simulated());
        with_test_source(0.5, || assert!(is_simulated()));
        assert!(!is_simulated());
    }

    #[test]
    fn a_pick_stays_in_range_and_is_inert_outside_a_simulation() {
        buggify_reset();
        assert_eq!(crate::buggify_pick!(1.0, 4), None);
        assert_eq!(crate::buggify_range!(1.0, 10..20), None);
        with_test_source(1.0, || {
            for _ in 0..200 {
                if let Some(i) = crate::buggify_pick!(1.0, 4) {
                    assert!(i < 4);
                }
                if let Some(v) = crate::buggify_range!(1.0, 10..20) {
                    assert!((10..20).contains(&v));
                }
            }
            assert_eq!(crate::buggify_pick!(1.0, 0), None);
            assert_eq!(crate::buggify_range!(1.0, 5..5), None);
        });
    }

    #[test]
    fn scale_covers_every_index() {
        assert_eq!(scale(0.0, 1), 0);
        assert_eq!(scale(0.0, 4), 0);
        assert_eq!(scale(0.25, 4), 1);
        assert_eq!(scale(0.999, 4), 3);
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
        with_test_source(0.5, || {
            let location = "consistent_location";
            let first = buggify_internal(1.0, location);
            let second = buggify_internal(1.0, location);
            // With prob=1.0 the outcome equals the activation decision, which is
            // made once per location.
            assert_eq!(first, second);
        });
    }

    #[test]
    fn sequences_replay_deterministically() {
        let run = || {
            with_test_source(0.5, || {
                (0..5)
                    .map(|i| {
                        let location = Box::leak(format!("loc_{i}").into_boxed_str());
                        buggify_internal(0.5, location)
                    })
                    .collect::<Vec<bool>>()
            })
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn always_active_always_fires() {
        let fired = with_test_source(1.0, || (0..20).any(|_| buggify_internal(1.0, "always")));
        assert!(fired, "activation 1.0 + prob 1.0 must fire");
    }

    #[test]
    fn a_named_location_follows_its_set_activation_whatever_its_draw() {
        with_test_source(0.0, || {
            // Activation 0.0: no location activates on its own.
            assert!(!crate::buggify_named!("unforced", 1.0));
            set_activation("forced", true);
            for _ in 0..20 {
                assert!(crate::buggify_named!("forced", 1.0));
            }
            // A new run clears the decision.
            buggify_init(0.0);
            assert!(!crate::buggify_named!("forced", 1.0));
        });
        with_test_source(1.0, || {
            // Activation 1.0: every location activates on its own, unless
            // the harness turned it off.
            set_activation("suppressed", false);
            for _ in 0..20 {
                assert!(!crate::buggify_named!("suppressed", 1.0));
            }
        });
    }

    #[test]
    fn a_named_location_is_silent_in_the_recovery_tail() {
        with_test_source(1.0, || {
            set_activation("tail", true);
            assert!(crate::buggify_named!("tail", 1.0));
            buggify_enter_recovery();
            assert!(!crate::buggify_named!("tail", 1.0));
        });
    }

    #[test]
    fn inert_outside_a_simulation() {
        buggify_reset();
        set_activation("outside", true);
        assert!(!crate::buggify_named!("outside", 1.0));
        buggify_reset();
    }

    #[test]
    fn reset_disables_firing() {
        with_test_source(1.0, || {
            assert!(buggify_internal(1.0, "reset_case"));
            buggify_reset();
            assert!(!buggify_internal(1.0, "reset_case"));
        });
    }

    #[test]
    fn recovery_silences_fault_sites_only() {
        with_test_source(1.0, || {
            assert!(crate::buggify_fault_with_prob!(1.0));
            buggify_enter_recovery();
            assert!(buggify_is_recovering());
            assert!(!crate::buggify_fault_with_prob!(1.0));
            assert!(crate::buggify_with_prob!(1.0), "ordinary sites keep firing");
            buggify_init(1.0);
            assert!(
                !buggify_is_recovering(),
                "a new run starts outside recovery"
            );
            assert!(crate::buggify_fault_with_prob!(1.0));
        });
    }

    #[test]
    fn macros_share_crate_state() {
        with_test_source(1.0, || {
            // buggify! fires at 25% per call; over many calls it must fire.
            assert!(
                (0..100).any(|_| crate::buggify!()),
                "macro must observe enabled state"
            );
            assert!(crate::buggify_with_prob!(1.0));
            buggify_reset();
            assert!((0..100).all(|_| !crate::buggify!()));
            assert!(!crate::buggify_with_prob!(1.0));
        });
    }
}
