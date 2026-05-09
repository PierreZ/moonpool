//! Cross-process invariant trait checked after every simulation event.
//!
//! Invariants are registered on the [`crate::SimulationBuilder`] and run by
//! the [`crate::observability::SimulationLayer`] each time an event is captured.
//! They receive a [`TimelineQuery`] view of all events seen so far and panic on
//! violation.

use super::query::TimelineQuery;

/// An invariant validated after every captured simulation event.
///
/// Invariants should panic with a descriptive message if violated. Use the
/// `assert_always!` macros from [`crate::chaos`] for structured failure reporting.
///
/// `Send` is required because invariants live inside a tracing `Subscriber`
/// which must be `Send + Sync`. moonpool runs single-threaded so this is a
/// type-system constraint only.
pub trait Invariant: 'static + Send {
    /// Name of this invariant, used in reports.
    fn name(&self) -> &str;

    /// Observe the latest captured events and validate properties.
    ///
    /// `q` exposes cursor-based incremental scans over typed timelines.
    /// `sim_time_ms` is the simulated time of the event that just fired.
    fn observe(&self, q: &dyn TimelineQuery, sim_time_ms: u64);

    /// Reset internal state at the start of each seed.
    ///
    /// Override to clear cursors and tracking sets. Default: no-op for
    /// stateless invariants.
    fn reset(&mut self) {}
}

type InvariantFn = Box<dyn Fn(&dyn TimelineQuery, u64) + Send>;

struct FnInvariant {
    name: String,
    check: InvariantFn,
}

impl Invariant for FnInvariant {
    fn name(&self) -> &str {
        &self.name
    }

    fn observe(&self, q: &dyn TimelineQuery, sim_time_ms: u64) {
        (self.check)(q, sim_time_ms);
    }
}

/// Wrap a closure as an [`Invariant`].
pub fn invariant_fn(
    name: impl Into<String>,
    f: impl Fn(&dyn TimelineQuery, u64) + Send + 'static,
) -> Box<dyn Invariant + Send> {
    Box::new(FnInvariant {
        name: name.into(),
        check: Box::new(f),
    })
}
