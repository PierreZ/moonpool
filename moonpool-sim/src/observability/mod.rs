//! Production-friendly observability layer for moonpool simulations.
//!
//! Replaces the legacy `Timeline<T>` + `Invariant` system with a custom
//! `tracing::Layer` that captures simulation events while letting the same
//! emission code paths reach production subscribers (fmt, OTel, etc.).
//!
//! # Architecture
//!
//! - Producers call [`crate::SimContext::emit`] (or the [`crate::sim_emit!`]
//!   macro for sim-internal sites). The macro stashes the typed payload in a
//!   thread-local and emits a `tracing::event!` at target `"moonpool::sim"`.
//! - The [`SimulationLayer`] (subscribed alongside any production layers)
//!   takes the typed payload from the thread-local at `on_event` time and
//!   stores it in a per-key vector along with `time_ms`, `source`, and a
//!   monotonic `seq`.
//! - After capturing each event, the layer runs all registered [`Invariant`]s.
//!   They see a [`TimelineQuery`] view supporting cursor-based incremental
//!   scans of typed entries.
//!
//! # Production
//!
//! With no [`SimulationLayer`] installed, the macro still emits a tracing event
//! so production subscribers see structured fields and a `Debug` payload. The
//! typed payload is not allocated. A relaxed atomic load gates the fast path.

pub mod emit;
pub mod event;
pub mod invariant;
pub mod layer;
pub mod query;

pub use event::{CapturedEvent, TypedEntry};
pub use invariant::{Invariant, invariant_fn};
pub use layer::{InstallGuard, SimulationLayer, SimulationLayerHandle};
pub use query::{TimelineQuery, TimelineQueryExt};

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Bring the generic helper trait into scope.
    use crate::observability::TimelineQueryExt;

    #[derive(Debug, Clone, PartialEq)]
    enum TestEvent {
        Hello(u64),
        World,
    }

    #[test]
    fn emit_without_layer_is_safe() {
        // No layer installed: macro still fires tracing::event! but does nothing else.
        let payload = TestEvent::Hello(7);
        crate::sim_emit!("test_key", 100, "10.0.1.1", payload);
    }

    #[test]
    fn layer_captures_typed_events() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        crate::sim_emit!("test_key", 100, "10.0.1.1", TestEvent::Hello(42));
        crate::sim_emit!("test_key", 200, "10.0.1.2", TestEvent::World);

        let entries = handle.timeline::<TestEvent>("test_key");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event, TestEvent::Hello(42));
        assert_eq!(entries[0].time_ms, 100);
        assert_eq!(entries[0].source, "10.0.1.1");
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].event, TestEvent::World);
        assert_eq!(entries[1].seq, 1);
    }

    #[test]
    fn invariants_run_after_each_event() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = count.clone();
        handle.register(invariant_fn("counter", move |q, _t| {
            count_clone.store(q.len("test_key"), Ordering::Relaxed);
        }));

        crate::sim_emit!("test_key", 1, "src", TestEvent::Hello(1));
        assert_eq!(count.load(Ordering::Relaxed), 1);
        crate::sim_emit!("test_key", 2, "src", TestEvent::Hello(2));
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    /// Send-and-Sync wrapper around `Cell` so tests can share a cursor between
    /// the test thread and the (notionally `Send`) invariant closure. moonpool
    /// runs single-threaded so this is sound.
    struct SendCell(Cell<usize>);
    unsafe impl Send for SendCell {}
    unsafe impl Sync for SendCell {}

    #[test]
    fn cursor_based_since_returns_only_new_events() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        let cursor = Arc::new(SendCell(Cell::new(0)));
        let cursor_clone = cursor.clone();
        let observed = Arc::new(AtomicUsize::new(0));
        let observed_clone = observed.clone();
        handle.register(invariant_fn("counter", move |q, _t| {
            let new = q.since::<TestEvent>("k", &cursor_clone.0);
            observed_clone.fetch_add(new.len(), Ordering::Relaxed);
        }));

        crate::sim_emit!("k", 1, "src", TestEvent::Hello(1));
        crate::sim_emit!("k", 2, "src", TestEvent::Hello(2));
        assert_eq!(observed.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn reset_for_seed_clears_events() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        crate::sim_emit!("k", 1, "src", TestEvent::Hello(1));
        assert_eq!(handle.snapshot_event_count(), 1);

        handle.reset_for_seed();
        assert_eq!(handle.snapshot_event_count(), 0);

        crate::sim_emit!("k", 2, "src", TestEvent::Hello(2));
        let entries = handle.timeline::<TestEvent>("k");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].seq, 0, "seq counter resets per seed");
    }

    #[test]
    #[should_panic(expected = "Invariant emitted")]
    fn invariant_emitting_event_panics() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        handle.register(invariant_fn("evil", |_q, _t| {
            crate::sim_emit!("k", 0, "src", TestEvent::World);
        }));

        crate::sim_emit!("k", 1, "src", TestEvent::Hello(1));
    }
}
