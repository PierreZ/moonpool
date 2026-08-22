//! Cross-process invariant trait checked after every simulation step.
//!
//! Invariants are registered on the [`crate::SimulationBuilder`] and run by
//! the orchestrator after each `sim.step()`. They receive a [`TraceQuery`]
//! view of all trace events captured so far and panic on violation.

use super::query::TraceQuery;

/// An invariant validated after every simulation step.
///
/// Invariants should report violations with the `assert_always!` macros from
/// [`crate::chaos`] for structured failure reporting.
///
/// `Send` is required because invariants are stored behind the layer handle,
/// which must be `Send + Sync`. moonpool runs single-threaded so this is a
/// type-system constraint only.
///
/// Invariants run from the orchestrator loop, not from inside tracing
/// dispatch, so emitting a `tracing` event from `observe` is captured like
/// any other — but treat invariants as read-only observers; emitting from a
/// checker blurs the line between system and oracle.
pub trait Invariant: 'static + Send {
    /// Name of this invariant, used in reports.
    fn name(&self) -> &str;

    /// Observe the latest captured events and validate properties.
    ///
    /// `q` exposes cursor-based incremental scans over events by name.
    /// `sim_time_ms` is the simulated time of the step that just completed.
    /// Calls batch events: one `observe` may see several new events.
    fn observe(&self, q: &dyn TraceQuery, sim_time_ms: u64);

    /// Reset internal state at the start of each seed.
    ///
    /// Override to clear cursors and tracking sets. Default: no-op for
    /// stateless invariants.
    fn reset(&mut self) {}
}

type InvariantFn = Box<dyn Fn(&dyn TraceQuery, u64) + Send>;

struct FnInvariant {
    name: String,
    check: InvariantFn,
}

impl Invariant for FnInvariant {
    fn name(&self) -> &str {
        &self.name
    }

    fn observe(&self, q: &dyn TraceQuery, sim_time_ms: u64) {
        (self.check)(q, sim_time_ms);
    }
}

/// Wrap a closure as an [`Invariant`].
pub fn invariant_fn(
    name: impl Into<String>,
    f: impl Fn(&dyn TraceQuery, u64) + Send + 'static,
) -> Box<dyn Invariant + Send> {
    Box::new(FnInvariant {
        name: name.into(),
        check: Box::new(f),
    })
}
