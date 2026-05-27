//! Captured event records and typed views.
//!
//! Events emitted as plain `tracing::*!` macros with the `capture = true` marker
//! field are picked up by [`crate::observability::SimulationLayer`] and stored
//! as [`CapturedEvent`] records. Invariants and inspection code read them as
//! [`TypedEntry<T>`] via cursor-based incremental queries that deserialize the
//! payload on demand.

use serde::de::DeserializeOwned;
use serde_json::Value;

/// A single event captured by the simulation layer.
///
/// The payload is stored as a [`serde_json::Value`] produced from the
/// `event = valuable(&p)` field at capture time. Invariants downcast via
/// [`crate::observability::TrailQueryExt::since`], which deserializes into
/// the requested type.
#[derive(Debug, Clone)]
pub struct CapturedEvent {
    /// Trail name (e.g. [`crate::SIM_FAULT_TRAIL`]).
    pub trail: String,
    /// Simulation time in milliseconds when the event was captured. Stamped
    /// by the layer from its current clock; not carried as a tracing field.
    pub time_ms: u64,
    /// Source identifier — process IP, `"sim"` for simulator-emitted events,
    /// or empty if no `source` field was supplied.
    pub source: String,
    /// Global monotonic sequence number across all trails.
    pub seq: u64,
    /// Serialized payload from the `event` field, deserializable into the
    /// caller-chosen `T` via [`TypedEntry::deserialize`].
    pub payload: Value,
}

/// A typed view of a captured event for use by invariants and tests.
#[derive(Debug, Clone)]
pub struct TypedEntry<T> {
    /// Deserialized payload.
    pub event: T,
    /// Simulation time in milliseconds when the event was captured.
    pub time_ms: u64,
    /// Source identifier (process IP, `"sim"`, or empty).
    pub source: String,
    /// Global monotonic sequence number across all trails.
    pub seq: u64,
}

impl<T: DeserializeOwned> TypedEntry<T> {
    /// Deserialize a [`CapturedEvent`] into a typed entry.
    ///
    /// Returns `None` if the payload does not deserialize as `T`.
    ///
    /// **Payload shape note:** `valuable-serde` emits enum unit variants as
    /// `{"VariantName": []}` while `serde`'s default external tagging expects
    /// the bare string `"VariantName"`. To avoid this round-trip mismatch,
    /// payload types should use **structs** or struct/tuple enum variants —
    /// not unit-variant enums. The `Heartbeat`/`LeaderElected`-style structs
    /// in `observability` tests and `tests/leader_election.rs` are the
    /// canonical shape.
    #[must_use]
    pub fn deserialize(e: &CapturedEvent) -> Option<Self> {
        let event: T = serde_json::from_value(e.payload.clone()).ok()?;
        Some(Self {
            event,
            time_ms: e.time_ms,
            source: e.source.clone(),
            seq: e.seq,
        })
    }
}
