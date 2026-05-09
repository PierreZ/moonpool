//! Read-side traits used by invariants to inspect captured events.
//!
//! [`TimelineQuery`] is dyn-safe (no generic methods) so invariants can take
//! `&dyn TimelineQuery`. The generic helpers live on
//! [`TimelineQueryExt`], which is auto-implemented for every `TimelineQuery`.
//! Cursor-based incremental scans match the original `Timeline::since` shape.

use std::cell::Cell;

use super::event::{CapturedEvent, TypedEntry};

/// Read-only view of captured events provided to invariants.
///
/// Dyn-safe core; for the generic `since::<T>` helper, see [`TimelineQueryExt`].
pub trait TimelineQuery {
    /// Total number of events captured under `key`.
    fn len(&self, key: &'static str) -> usize;

    /// Highest sequence number assigned so far. Returns 0 if no events yet.
    fn last_seq(&self) -> u64;

    /// Drain new captured events under `key` past `cursor`, advance `cursor`
    /// to the new end, and return clones.
    fn drain_since(&self, key: &'static str, cursor: &Cell<usize>) -> Vec<CapturedEvent>;
}

/// Generic helpers on top of [`TimelineQuery`].
///
/// Provides the typed `since::<T>` and `snapshot::<T>` APIs. Auto-implemented
/// for every type that implements [`TimelineQuery`], including `dyn TimelineQuery`.
pub trait TimelineQueryExt: TimelineQuery {
    /// Drain new typed events under `key` past `cursor`, advance `cursor`,
    /// and downcast to `T`.
    ///
    /// Entries whose payload type does not match `T` are skipped. The cursor
    /// is always advanced to the current length of the timeline.
    fn since<T: 'static + Clone>(
        &self,
        key: &'static str,
        cursor: &Cell<usize>,
    ) -> Vec<TypedEntry<T>> {
        self.drain_since(key, cursor)
            .into_iter()
            .filter_map(|e| {
                e.payload.as_any().downcast_ref::<T>().map(|t| TypedEntry {
                    event: t.clone(),
                    time_ms: e.time_ms,
                    source: e.source.clone(),
                    seq: e.seq,
                })
            })
            .collect()
    }

    /// Snapshot all typed events under `key`. Convenience for full re-scans;
    /// prefer [`since`](Self::since) when an incremental cursor will do.
    fn snapshot<T: 'static + Clone>(&self, key: &'static str) -> Vec<TypedEntry<T>> {
        let cursor = Cell::new(0);
        self.since::<T>(key, &cursor)
    }
}

impl<Q: TimelineQuery + ?Sized> TimelineQueryExt for Q {}
