//! Captured event records and typed views.
//!
//! Events emitted via [`crate::sim_emit!`] are captured by the [`crate::observability::SimulationLayer`]
//! and stored as [`CapturedEvent`] records. Invariants and inspection code read
//! them as [`TypedEntry<T>`] via cursor-based incremental queries.

use std::any::Any;
use std::sync::Arc;

/// Trait for typed event payloads. Auto-implemented for all `T: Any + Send + Sync`.
///
/// `Send + Sync` is required because captured events live inside a
/// `tracing::Subscriber`, which must be `Send + Sync`. moonpool simulations
/// run single-threaded so this is purely a type-system requirement; no actual
/// cross-thread access happens at runtime.
///
/// The `as_any` method enables downcasting back to the concrete `T` since
/// trait objects of `AnyPayload` cannot be downcast directly.
pub trait AnyPayload: Send + Sync + 'static {
    /// Return a reference as `&dyn Any` to enable type-safe downcasting.
    fn as_any(&self) -> &dyn Any;
}

impl<T: Any + Send + Sync> AnyPayload for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A single event captured by the simulation layer.
///
/// Stored internally; invariants typically read these via the
/// [`crate::observability::TimelineQueryExt::since`] generic helper which
/// downcasts the payload into [`TypedEntry<T>`] values.
#[derive(Clone)]
pub struct CapturedEvent {
    /// Timeline key (e.g. [`crate::SIM_FAULT_TIMELINE`]).
    pub key: &'static str,
    /// Simulation time in milliseconds when the event was emitted.
    pub time_ms: u64,
    /// Source identifier — process IP or `"sim"` for simulator-emitted events.
    pub source: String,
    /// Global monotonic sequence number across all timelines.
    pub seq: u64,
    /// Typed payload. Downcast via `Any::downcast_ref` in the consumer.
    pub payload: Arc<dyn AnyPayload>,
}

impl std::fmt::Debug for CapturedEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedEvent")
            .field("key", &self.key)
            .field("time_ms", &self.time_ms)
            .field("source", &self.source)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

/// A typed view of a captured event for use by invariants and tests.
#[derive(Debug, Clone)]
pub struct TypedEntry<T> {
    /// Cloned payload of the captured event.
    pub event: T,
    /// Simulation time in milliseconds when the event was emitted.
    pub time_ms: u64,
    /// Source identifier (process IP or `"sim"`).
    pub source: String,
    /// Global monotonic sequence number across all timelines.
    pub seq: u64,
}
