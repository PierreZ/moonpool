//! Read-side traits used by invariants to inspect captured events.
//!
//! [`TrailQuery`] is dyn-safe (no generic methods) so invariants can take
//! `&dyn TrailQuery`. The generic helpers live on [`TrailQueryExt`], which is
//! auto-implemented for every `TrailQuery`. Cursor-based incremental scans
//! deserialize the payload into the caller-chosen `T` on demand.

use std::cell::Cell;

use serde::de::DeserializeOwned;

use super::event::{CapturedEvent, TypedEntry};

/// Read-only view of captured events provided to invariants.
///
/// Dyn-safe core; for the generic `since::<T>` helper, see [`TrailQueryExt`].
pub trait TrailQuery {
    /// Total number of events captured under `trail`.
    fn len(&self, trail: &str) -> usize;

    /// Highest sequence number assigned so far. Returns 0 if no events yet.
    fn last_seq(&self) -> u64;

    /// Drain new captured events under `trail` past `cursor`, advance `cursor`
    /// to the new end, and return clones.
    fn drain_since(&self, trail: &str, cursor: &Cell<usize>) -> Vec<CapturedEvent>;
}

/// Generic helpers on top of [`TrailQuery`].
///
/// Provides the typed `since::<T>` and `snapshot::<T>` APIs. Auto-implemented
/// for every type that implements [`TrailQuery`], including `dyn TrailQuery`.
pub trait TrailQueryExt: TrailQuery {
    /// Drain new typed events under `trail` past `cursor`, advance `cursor`,
    /// and deserialize each payload into `T`.
    ///
    /// Entries whose payload does not deserialize as `T` are skipped. The
    /// cursor is always advanced to the current length of the trail.
    fn since<T: DeserializeOwned>(&self, trail: &str, cursor: &Cell<usize>) -> Vec<TypedEntry<T>> {
        self.drain_since(trail, cursor)
            .iter()
            .filter_map(TypedEntry::<T>::deserialize)
            .collect()
    }

    /// Snapshot all typed events under `trail`. Convenience for full re-scans;
    /// prefer [`since`](Self::since) when an incremental cursor will do.
    fn snapshot<T: DeserializeOwned>(&self, trail: &str) -> Vec<TypedEntry<T>> {
        let cursor = Cell::new(0);
        self.since::<T>(trail, &cursor)
    }
}

impl<Q: TrailQuery + ?Sized> TrailQueryExt for Q {}
