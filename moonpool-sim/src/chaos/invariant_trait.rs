//! Trait-based invariant checking for simulation correctness verification.
//!
//! Invariants validate cross-workload properties after every simulation event.
//! They receive a [`StateHandle`] containing workload-published state and
//! the current simulation time.
//!
//! # Usage
//!
//! Implement the trait directly or use the [`invariant_fn`] closure adapter:
//!
//! ```ignore
//! use moonpool_sim::{Invariant, invariant_fn, StateHandle};
//!
//! // Trait implementation
//! struct ConservationLaw;
//! impl Invariant for ConservationLaw {
//!     fn name(&self) -> &str { "conservation" }
//!     fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
//!         // validate...
//!     }
//! }
//!
//! // Closure shorthand
//! let inv = invariant_fn("no_negative_balance", |state, _t| {
//!     // validate...
//! });
//! ```

use super::state_handle::StateHandle;

/// An invariant that validates system-wide properties during simulation.
///
/// Invariants are checked after every simulation event. They should panic
/// with a descriptive message if the invariant is violated.
pub trait Invariant: 'static {
    /// Name of this invariant for reporting.
    fn name(&self) -> &str;

    /// Check the invariant against current state and simulation time.
    ///
    /// Should panic with a descriptive message if the invariant is violated.
    fn check(&self, state: &StateHandle, sim_time_ms: u64);
}

/// Type alias for invariant check closures.
type InvariantCheckFn = Box<dyn Fn(&StateHandle, u64)>;

/// Closure-based invariant adapter.
struct FnInvariant {
    name: String,
    check_fn: InvariantCheckFn,
}

impl Invariant for FnInvariant {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(&self, state: &StateHandle, sim_time_ms: u64) {
        (self.check_fn)(state, sim_time_ms);
    }
}

/// Create an invariant from a closure.
///
/// # Example
///
/// ```ignore
/// let inv = invariant_fn("balance_positive", |state, _t| {
///     let balance: i64 = state.get("balance").unwrap_or(0);
///     assert!(balance >= 0, "balance went negative: {}", balance);
/// });
/// ```
pub fn invariant_fn(
    name: impl Into<String>,
    f: impl Fn(&StateHandle, u64) + 'static,
) -> Box<dyn Invariant> {
    Box::new(FnInvariant {
        name: name.into(),
        check_fn: Box::new(f),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestInvariant;

    impl Invariant for TestInvariant {
        fn name(&self) -> &str {
            "test"
        }

        fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
            if let Some(val) = state.get::<i64>("value") {
                assert!(val >= 0, "value went negative: {}", val);
            }
        }
    }

    #[test]
    fn test_trait_impl() {
        let inv = TestInvariant;
        let state = StateHandle::new();
        state.publish("value", 42i64);
        inv.check(&state, 0);
        assert_eq!(inv.name(), "test");
    }

    #[test]
    fn test_invariant_fn() {
        let inv = invariant_fn("check_positive", |state, _t| {
            if let Some(val) = state.get::<i64>("val") {
                assert!(val >= 0, "negative: {}", val);
            }
        });
        let state = StateHandle::new();
        state.publish("val", 10i64);
        inv.check(&state, 100);
        assert_eq!(inv.name(), "check_positive");
    }

    #[test]
    #[should_panic(expected = "negative")]
    fn test_invariant_violation_panics() {
        let inv = invariant_fn("must_be_positive", |state, _t| {
            let val: i64 = state.get("val").unwrap_or(0);
            assert!(val >= 0, "negative: {}", val);
        });
        let state = StateHandle::new();
        state.publish("val", -1i64);
        inv.check(&state, 0);
    }
}
