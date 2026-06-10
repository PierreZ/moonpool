//! Captured trace events and field values.
//!
//! Plain `tracing` events emitted by processes and workloads are picked up by
//! [`crate::observability::SimulationLayer`] and stored as [`TraceEvent`]
//! records — the same shape a production log pipeline would see. Invariants
//! and inspection code read them through [`crate::observability::TraceQuery`]
//! and extract typed fields by key.

use std::collections::BTreeMap;

/// A single structured field value captured from a `tracing` event.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// Boolean field.
    Bool(bool),
    /// Signed integer field.
    I64(i64),
    /// Unsigned integer field.
    U64(u64),
    /// Floating-point field.
    F64(f64),
    /// String field. Also produced by `%`-formatted (`Display`) and
    /// `?`-formatted (`Debug`) values.
    Str(String),
}

/// One captured trace event — the same shape as a production log line.
#[derive(Debug, Clone)]
pub struct TraceEvent {
    /// Global monotonic sequence number across all event names. Reset per seed.
    pub seq: u64,
    /// Simulation time in milliseconds when the event was captured.
    pub time_ms: u64,
    /// Originating actor: the `ip` recorded on the nearest enclosing process
    /// or workload span, or `"sim"` for runner-injected fault events.
    pub source: String,
    /// The `tracing` target (defaults to the emitting module path).
    pub target: String,
    /// Event severity level.
    pub level: tracing::Level,
    /// Event name: the `tracing` message, e.g. `"leader_elected"`.
    pub name: String,
    /// Structured fields recorded on the event.
    pub fields: BTreeMap<String, FieldValue>,
}

impl TraceEvent {
    /// Read field `key` as a `u64`. Also accepts non-negative `i64` values.
    #[must_use]
    pub fn u64(&self, key: &str) -> Option<u64> {
        match self.fields.get(key)? {
            FieldValue::U64(v) => Some(*v),
            FieldValue::I64(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }

    /// Read field `key` as an `i64`. Also accepts `u64` values that fit.
    #[must_use]
    pub fn i64(&self, key: &str) -> Option<i64> {
        match self.fields.get(key)? {
            FieldValue::I64(v) => Some(*v),
            FieldValue::U64(v) => i64::try_from(*v).ok(),
            _ => None,
        }
    }

    /// Read field `key` as an `f64`.
    #[must_use]
    pub fn f64(&self, key: &str) -> Option<f64> {
        match self.fields.get(key)? {
            FieldValue::F64(v) => Some(*v),
            _ => None,
        }
    }

    /// Read field `key` as a `bool`.
    #[must_use]
    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.fields.get(key)? {
            FieldValue::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// Read field `key` as a string slice.
    ///
    /// Values recorded with `%` (`Display`) arrive unquoted; values recorded
    /// with `?` (`Debug`) keep their `Debug` formatting (a `String` recorded
    /// with `?` includes quotes — prefer `%` for strings).
    #[must_use]
    pub fn str(&self, key: &str) -> Option<&str> {
        match self.fields.get(key)? {
            FieldValue::Str(v) => Some(v),
            _ => None,
        }
    }
}
