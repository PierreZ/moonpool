//! Trait-based invariant system for simulation testing.
//!
//! The `Invariant` trait replaces the old `InvariantCheck` type alias with a
//! more structured interface. Invariants are checked after every simulation
//! event and should panic on violation (crash-early philosophy).

use super::state_handle::StateHandle;

/// A named invariant that validates cross-workload properties.
///
/// Invariants are checked after every simulation event. They should panic
/// if a violation is detected (FoundationDB's "crash early" philosophy).
///
/// # Example
///
/// ```ignore
/// struct ConservationLaw;
///
/// impl Invariant for ConservationLaw {
///     fn name(&self) -> &str { "conservation_law" }
///     fn check(&self, state: &StateHandle, sim_time: u64) {
///         let deposited = state.get::<u64>("total_deposited").unwrap_or(0);
///         let withdrawn = state.get::<u64>("total_withdrawn").unwrap_or(0);
///         let balance = state.get::<i64>("total_balance").unwrap_or(0);
///         assert_eq!(balance, deposited as i64 - withdrawn as i64);
///     }
/// }
/// ```
pub trait Invariant {
    /// The human-readable name of this invariant.
    fn name(&self) -> &str;

    /// Check this invariant against the current shared state.
    ///
    /// Should panic if the invariant is violated. The `sim_time` parameter
    /// is the current simulation time in milliseconds.
    fn check(&self, state: &StateHandle, sim_time: u64);
}

/// Create a boxed invariant from a name and closure.
///
/// Convenience adapter for creating invariants without defining a struct.
///
/// # Example
///
/// ```ignore
/// let inv = invariant_fn("no_negative_balance", |state, _time| {
///     let balance = state.get::<i64>("balance").unwrap_or(0);
///     assert!(balance >= 0, "Balance went negative: {}", balance);
/// });
/// ```
pub fn invariant_fn<F>(name: &str, check: F) -> Box<dyn Invariant>
where
    F: Fn(&StateHandle, u64) + 'static,
{
    Box::new(FnInvariant {
        name: name.to_string(),
        check,
    })
}

struct FnInvariant<F> {
    name: String,
    check: F,
}

impl<F> Invariant for FnInvariant<F>
where
    F: Fn(&StateHandle, u64),
{
    fn name(&self) -> &str {
        &self.name
    }

    fn check(&self, state: &StateHandle, sim_time: u64) {
        (self.check)(state, sim_time);
    }
}
