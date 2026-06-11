//! Read-side trait used by invariants to inspect captured trace events.
//!
//! [`TraceQuery`] is dyn-safe so invariants can take `&dyn TraceQuery`.
//! Events are keyed by their name (the `tracing` message), mirroring how a
//! production trace store is queried by event name. Typed values are
//! extracted per field via the accessors on
//! [`TraceEvent`](super::event::TraceEvent).

use std::cell::Cell;

use super::event::TraceEvent;

/// Read-only view of captured trace events provided to invariants.
pub trait TraceQuery {
    /// Total number of events captured under `name`.
    fn len(&self, name: &str) -> usize;

    /// Return clones of events named `name` past `cursor`, and advance the
    /// cursor to the new end.
    ///
    /// Cursors index into the per-name event list and stay stable within a
    /// seed; reset them in [`Invariant::reset`](super::Invariant::reset).
    fn since(&self, name: &str, cursor: &Cell<usize>) -> Vec<TraceEvent>;

    /// All events captured under `name` since the start of the seed.
    /// Convenience for full re-scans; prefer [`since`](Self::since) when an
    /// incremental cursor will do.
    fn snapshot(&self, name: &str) -> Vec<TraceEvent>;
}
