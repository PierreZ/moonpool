//! Type-safe shared state for workload communication and invariant checking.
//!
//! `StateHandle` replaces the JSON-based `StateRegistry` with a type-safe
//! `Rc<RefCell<HashMap>>` backed store using `Box<dyn Any>`.
//!
//! # Usage
//!
//! ```ignore
//! use moonpool_sim::StateHandle;
//!
//! let state = StateHandle::new();
//! state.publish("counter", 42u64);
//! let val: Option<u64> = state.get("counter");
//! assert_eq!(val, Some(42));
//! ```

use std::any::Any;
use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// A single entry in an append-only event timeline.
#[derive(Debug, Clone)]
pub struct TimelineEntry<T> {
    /// The event payload.
    pub event: T,
    /// Simulation time in milliseconds when the event was emitted.
    pub time_ms: u64,
    /// IP address of the emitter.
    pub source: String,
    /// Global sequence number (monotonically increasing across all timelines).
    pub seq: u64,
}

/// Handle to a typed, append-only event timeline.
///
/// Clone is cheap (Rc-based). All clones share the same underlying storage.
#[derive(Clone)]
pub struct Timeline<T: 'static> {
    inner: Rc<RefCell<Vec<TimelineEntry<T>>>>,
}

impl<T: 'static> Timeline<T> {
    /// Number of entries in this timeline.
    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    /// Whether this timeline has no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().is_empty()
    }

    /// Borrow all entries (zero-copy).
    pub fn all(&self) -> Ref<'_, Vec<TimelineEntry<T>>> {
        self.inner.borrow()
    }
}

impl<T: Clone + 'static> Timeline<T> {
    /// Get entries from `index` onward (for cursor-based incremental invariants).
    pub fn since(&self, index: usize) -> Vec<TimelineEntry<T>> {
        self.inner.borrow()[index..].to_vec()
    }

    /// Get the last entry, if any.
    pub fn last(&self) -> Option<TimelineEntry<T>> {
        self.inner.borrow().last().cloned()
    }
}

/// Shared state handle for cross-workload communication and invariant checking.
///
/// Provides type-safe publish/get semantics using `std::any::Any` for type erasure.
/// Clone is cheap (Rc-based) — all clones share the same underlying storage.
#[derive(Clone)]
pub struct StateHandle {
    inner: Rc<RefCell<HashMap<String, Box<dyn Any>>>>,
    timelines: Rc<RefCell<HashMap<String, Box<dyn Any>>>>,
    next_seq: Rc<Cell<u64>>,
}

impl Default for StateHandle {
    fn default() -> Self {
        Self {
            inner: Rc::default(),
            timelines: Rc::default(),
            next_seq: Rc::new(Cell::new(0)),
        }
    }
}

impl StateHandle {
    /// Create a new empty state handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a value under a key, replacing any existing value.
    pub fn publish<T: Any + 'static>(&self, key: &str, value: T) {
        self.inner
            .borrow_mut()
            .insert(key.to_string(), Box::new(value));
    }

    /// Get a cloned copy of the value under a key, if it exists and matches the type.
    ///
    /// Returns `None` if the key doesn't exist or the type doesn't match.
    pub fn get<T: Any + Clone + 'static>(&self, key: &str) -> Option<T> {
        self.inner
            .borrow()
            .get(key)
            .and_then(|v| v.downcast_ref::<T>())
            .cloned()
    }

    /// Check whether a key exists in the state.
    pub fn contains(&self, key: &str) -> bool {
        self.inner.borrow().contains_key(key)
    }

    /// Append an event to a named timeline with explicit metadata.
    ///
    /// Prefer [`SimContext::emit`] which auto-captures time and source IP.
    pub fn emit_raw<T: Any + 'static>(&self, key: &str, event: T, time_ms: u64, source: &str) {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        let entry = TimelineEntry {
            event,
            time_ms,
            source: source.to_string(),
            seq,
        };

        // Get or create the Rc<RefCell<Vec>> for this key, then drop the map borrow
        // before pushing to avoid nested RefCell borrows.
        let rc = {
            let mut map = self.timelines.borrow_mut();
            let boxed = map
                .entry(key.to_string())
                .or_insert_with(|| Box::new(Rc::new(RefCell::new(Vec::<TimelineEntry<T>>::new()))));
            boxed
                .downcast_ref::<Rc<RefCell<Vec<TimelineEntry<T>>>>>()
                .expect("timeline type mismatch: key already used with a different type")
                .clone()
        };
        rc.borrow_mut().push(entry);
    }

    /// Get a typed timeline by name.
    ///
    /// Returns `None` if no events have been emitted to this key,
    /// or if the type doesn't match.
    pub fn timeline<T: Any + 'static>(&self, key: &str) -> Option<Timeline<T>> {
        let map = self.timelines.borrow();
        let boxed = map.get(key)?;
        let rc = boxed.downcast_ref::<Rc<RefCell<Vec<TimelineEntry<T>>>>>()?;
        Some(Timeline { inner: rc.clone() })
    }
}

impl std::fmt::Debug for StateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state_count = self.inner.borrow().len();
        let timeline_count = self.timelines.borrow().len();
        f.debug_struct("StateHandle")
            .field("state_keys", &state_count)
            .field("timeline_keys", &timeline_count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_publish_and_get() {
        let state = StateHandle::new();
        state.publish("count", 42u64);
        assert_eq!(state.get::<u64>("count"), Some(42));
    }

    #[test]
    fn test_get_wrong_type_returns_none() {
        let state = StateHandle::new();
        state.publish("count", 42u64);
        assert_eq!(state.get::<String>("count"), None);
    }

    #[test]
    fn test_get_missing_key_returns_none() {
        let state = StateHandle::new();
        assert_eq!(state.get::<u64>("missing"), None);
    }

    #[test]
    fn test_contains() {
        let state = StateHandle::new();
        assert!(!state.contains("key"));
        state.publish("key", "value".to_string());
        assert!(state.contains("key"));
    }

    #[test]
    fn test_publish_replaces() {
        let state = StateHandle::new();
        state.publish("x", 1u64);
        state.publish("x", 2u64);
        assert_eq!(state.get::<u64>("x"), Some(2));
    }

    #[test]
    fn test_clone_shares_state() {
        let state = StateHandle::new();
        let clone = state.clone();
        state.publish("shared", true);
        assert_eq!(clone.get::<bool>("shared"), Some(true));
    }

    #[test]
    fn test_emit_and_read_timeline() {
        let state = StateHandle::new();
        state.emit_raw("ops", "first", 100, "10.0.1.1");
        state.emit_raw("ops", "second", 200, "10.0.1.2");
        state.emit_raw("ops", "third", 300, "10.0.1.1");

        let tl = state
            .timeline::<&str>("ops")
            .expect("timeline should exist");
        assert_eq!(tl.len(), 3);

        let entries = tl.all();
        assert_eq!(entries[0].event, "first");
        assert_eq!(entries[0].time_ms, 100);
        assert_eq!(entries[0].source, "10.0.1.1");
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].event, "second");
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[2].event, "third");
        assert_eq!(entries[2].seq, 2);
    }

    #[test]
    fn test_timeline_missing_key_returns_none() {
        let state = StateHandle::new();
        assert!(state.timeline::<u64>("missing").is_none());
    }

    #[test]
    fn test_timeline_wrong_type_returns_none() {
        let state = StateHandle::new();
        state.emit_raw("ops", 42u64, 0, "10.0.1.1");
        assert!(state.timeline::<String>("ops").is_none());
    }

    #[test]
    fn test_timeline_since() {
        let state = StateHandle::new();
        for i in 0..5u64 {
            state.emit_raw("events", i, i * 10, "10.0.1.1");
        }

        let tl = state.timeline::<u64>("events").expect("should exist");
        let tail = tl.since(3);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].event, 3);
        assert_eq!(tail[1].event, 4);
    }

    #[test]
    fn test_timeline_clone_shares_data() {
        let state = StateHandle::new();
        state.emit_raw("log", "a", 0, "10.0.1.1");

        let tl = state.timeline::<&str>("log").expect("should exist");
        let tl2 = tl.clone();

        state.emit_raw("log", "b", 10, "10.0.1.2");
        assert_eq!(tl.len(), 2);
        assert_eq!(tl2.len(), 2);
    }

    #[test]
    fn test_seq_spans_timelines() {
        let state = StateHandle::new();
        state.emit_raw("a", 1u64, 0, "10.0.1.1");
        state.emit_raw("b", 2u64, 0, "10.0.1.1");

        let tl_a = state.timeline::<u64>("a").expect("a");
        let tl_b = state.timeline::<u64>("b").expect("b");
        assert_eq!(tl_a.all()[0].seq, 0);
        assert_eq!(tl_b.all()[0].seq, 1);
    }

    #[test]
    fn test_timeline_last() {
        let state = StateHandle::new();
        state.emit_raw("log", "first", 0, "10.0.1.1");
        state.emit_raw("log", "second", 10, "10.0.1.1");

        let tl = state.timeline::<&str>("log").expect("should exist");
        let last = tl.last().expect("should have last");
        assert_eq!(last.event, "second");
        assert_eq!(last.time_ms, 10);
    }
}
