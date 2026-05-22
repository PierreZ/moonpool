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
pub mod fmt;
pub mod init;
pub mod invariant;
pub mod layer;
pub mod query;

pub use event::{CapturedEvent, TypedEntry};
pub use fmt::{Clock, SimTime};
pub use init::init_sim_tracing;
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

        handle.reset_for_seed();

        crate::sim_emit!("k", 2, "src", TestEvent::Hello(2));
        let entries = handle.timeline::<TestEvent>("k");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].seq, 0, "seq counter resets per seed");
    }

    #[test]
    fn sim_emit_updates_layer_sim_time() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        crate::sim_emit!("k", 12345, "src", TestEvent::Hello(1));
        assert_eq!(handle.current_sim_time_ms(), 12345);

        crate::sim_emit!("k", 99000, "src", TestEvent::Hello(2));
        assert_eq!(handle.current_sim_time_ms(), 99000);
    }

    #[test]
    fn handle_set_sim_time_for_non_emit_paths() {
        // The orchestrator advances the layer's clock between events so
        // unrelated `tracing::*!` calls also see the right sim time.
        let layer = SimulationLayer::new();
        let handle = layer.handle();
        handle.set_sim_time_ms(42_000);
        assert_eq!(handle.current_sim_time_ms(), 42_000);
    }

    #[test]
    fn sim_time_format_writes_seconds_and_millis() {
        use tracing_subscriber::fmt::format::Writer;
        use tracing_subscriber::fmt::time::FormatTime;

        let layer = SimulationLayer::new();
        let handle = layer.handle();
        handle.set_sim_time_ms(7042);

        let st = SimTime::new(handle.clone());
        let mut buf = String::new();
        let mut writer = Writer::new(&mut buf);
        st.format_time(&mut writer)
            .expect("writing to a String never fails");
        assert_eq!(buf, "sim+    7.042s");
    }

    #[test]
    fn clock_trait_can_be_implemented_for_a_stub() {
        // Demonstrates that `Clock` is the formatter's only dependency on a
        // time source — alternate impls (e.g. test stubs) work without a layer.
        use tracing_subscriber::fmt::format::Writer;
        use tracing_subscriber::fmt::time::FormatTime;

        struct FixedClock(u64);
        impl crate::observability::Clock for FixedClock {
            fn now_ms(&self) -> u64 {
                self.0
            }
        }

        let st = SimTime::new(FixedClock(123_456));
        let mut buf = String::new();
        let mut writer = Writer::new(&mut buf);
        st.format_time(&mut writer)
            .expect("writing to a String never fails");
        assert_eq!(buf, "sim+  123.456s");
    }

    #[test]
    fn fmt_layer_with_sim_time_prefixes_log_output() {
        // End-to-end: register a real fmt::Layer with SimTime as its timer
        // alongside SimulationLayer, then verify a `tracing::info!` is
        // prefixed with the layer's current sim time.
        use std::io;
        use std::sync::Mutex;

        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct VecWriter(Arc<Mutex<Vec<u8>>>);

        impl io::Write for VecWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .expect("test writer poisoned")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
            type Writer = VecWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let layer = SimulationLayer::new();
        let handle = layer.handle();
        handle.set_sim_time_ms(5_000);

        let writer = VecWriter::default();
        let buf = writer.0.clone();

        // Order matters: SimulationLayer must precede fmt so its on_event
        // updates `current_sim_time_ms` before fmt formats the event.
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_timer(SimTime::new(handle.clone()))
            .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
        let subscriber = tracing_subscriber::registry().with(layer).with(fmt_layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("hello at 5s");
            // Advance the layer clock as the orchestrator would, then emit
            // an unrelated tracing event — fmt should see the new sim time.
            handle.set_sim_time_ms(12_345);
            tracing::info!("hello at 12.345s");
        });

        let output = String::from_utf8(buf.lock().expect("buf").clone()).expect("utf-8 fmt output");
        assert!(
            output.contains("sim+    5.000s"),
            "expected sim+5s prefix; got: {output}"
        );
        assert!(
            output.contains("sim+   12.345s"),
            "expected sim+12.345s prefix after clock advance; got: {output}"
        );
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
