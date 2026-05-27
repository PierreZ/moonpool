//! Production-friendly observability layer for moonpool simulations.
//!
//! Captures correctness-relevant events emitted as ordinary `tracing` events
//! and runs registered [`Invariant`]s against them after each capture. The
//! same emission API works in simulation AND in production: any subscriber —
//! including [`SimulationLayer`] — can consume the events.
//!
//! # Emission convention
//!
//! Use a standard `tracing::*!` macro with three structured fields:
//!
//! - `capture = true` — the marker; events without it are ignored by the layer.
//! - `trail = "name"` — selects which append-only stream the event joins.
//! - `event = valuable(&payload)` — typed payload via the [`valuable`] crate.
//!
//! An optional `source = "..."` field records the originating actor (e.g. a
//! process IP or `"sim"`). Sim time is stamped automatically from the
//! orchestrator-pushed clock; do not include it as a field.
//!
//! ```ignore
//! use serde::{Deserialize, Serialize};
//! use tracing::field::valuable;
//! use valuable::Valuable;
//!
//! #[derive(Valuable, Serialize, Deserialize, Clone)]
//! struct LeaderElected { term: u64, leader: String }
//!
//! tracing::info!(
//!     capture = true,
//!     trail = "leader",
//!     source = node_ip,
//!     event = valuable(&LeaderElected { term: 5, leader: node_ip.to_string() }),
//!     "elected",
//! );
//! ```
//!
//! Invariants read the trail via [`TrailQueryExt::since`]:
//!
//! ```ignore
//! for entry in q.since::<LeaderElected>("leader", &cursor) {
//!     // panic on split-brain, validate term monotonicity, etc.
//! }
//! ```
//!
//! # Payload type guidance
//!
//! Payload types should be **structs** or struct/tuple enum variants. Avoid
//! unit-variant enums: `valuable-serde` emits them as `{"VariantName": []}`
//! while `serde`'s default external tagging deserializes from the bare string
//! `"VariantName"`, so the round-trip silently fails. Add at least one field
//! (e.g. a `reason: String` for catch-all variants) and the shape is
//! unambiguous in both directions.

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
pub use layer::{InstallGuard, SimulationLayer, SimulationLayerHandle, layer_installed};
pub use query::{TrailQuery, TrailQueryExt};

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use serde::{Deserialize, Serialize};
    use tracing::field::valuable;
    use valuable::Valuable;

    use crate::observability::TrailQueryExt;

    /// Realistic correctness fact used by the capture-path tests. A `Heartbeat`
    /// is the kind of payload a user actually emits: a struct with named
    /// fields that round-trips cleanly through valuable-serde and serde.
    #[derive(Debug, Clone, PartialEq, Valuable, Serialize, Deserialize)]
    struct Heartbeat {
        node: String,
        term: u64,
    }

    fn heartbeat(node: &str, term: u64) -> Heartbeat {
        Heartbeat {
            node: node.into(),
            term,
        }
    }

    // ------------------------------------------------------------------
    // Capture-path tests
    // ------------------------------------------------------------------

    #[test]
    fn emit_without_layer_is_safe() {
        // No layer installed: tracing::info! does not panic and there's
        // nothing to observe. We just check the macro compiles and runs.
        tracing::info!(
            capture = true,
            trail = "hb",
            source = "10.0.1.1",
            event = valuable(&heartbeat("n1", 1)),
        );
    }

    #[test]
    fn layer_captures_marked_event() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        handle.set_sim_time_ms(1234);
        tracing::info!(
            capture = true,
            trail = "hb",
            source = "10.0.1.1",
            event = valuable(&heartbeat("n1", 7)),
        );

        let entries = handle.trail::<Heartbeat>("hb");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, heartbeat("n1", 7));
        assert_eq!(entries[0].source, "10.0.1.1");
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[0].time_ms, 1234);
    }

    #[test]
    fn event_missing_capture_marker_is_ignored() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        // Note: no `capture = true`.
        tracing::info!(trail = "hb", event = valuable(&heartbeat("n1", 1)));

        assert!(handle.trail::<Heartbeat>("hb").is_empty());
    }

    #[test]
    fn event_missing_trail_is_ignored() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        tracing::info!(capture = true, event = valuable(&heartbeat("n1", 1)));

        // Without a trail name there's no key to read under.
        assert!(handle.trail::<Heartbeat>("").is_empty());
        assert!(handle.trail::<Heartbeat>("hb").is_empty());
    }

    #[test]
    fn event_missing_payload_is_ignored() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        tracing::info!(capture = true, trail = "hb", "no payload");

        assert!(handle.trail::<Heartbeat>("hb").is_empty());
    }

    #[test]
    fn multiple_trails_are_independent() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        tracing::info!(
            capture = true,
            trail = "a",
            event = valuable(&heartbeat("n1", 1))
        );
        tracing::info!(
            capture = true,
            trail = "b",
            event = valuable(&heartbeat("n2", 2))
        );
        tracing::info!(
            capture = true,
            trail = "a",
            event = valuable(&heartbeat("n3", 3))
        );

        let a = handle.trail::<Heartbeat>("a");
        let b = handle.trail::<Heartbeat>("b");
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
        assert_eq!(a[0].event.node, "n1");
        assert_eq!(a[1].event.node, "n3");
        assert_eq!(b[0].event.node, "n2");
    }

    #[test]
    fn seq_is_monotonic_across_trails() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        tracing::info!(
            capture = true,
            trail = "a",
            event = valuable(&heartbeat("n", 1))
        );
        tracing::info!(
            capture = true,
            trail = "b",
            event = valuable(&heartbeat("n", 2))
        );
        tracing::info!(
            capture = true,
            trail = "a",
            event = valuable(&heartbeat("n", 3))
        );

        let a = handle.trail::<Heartbeat>("a");
        let b = handle.trail::<Heartbeat>("b");
        assert_eq!(a[0].seq, 0);
        assert_eq!(b[0].seq, 1);
        assert_eq!(a[1].seq, 2);
    }

    /// Send+Sync wrapper around `Cell` for sharing cursors with a `Send`
    /// invariant closure. moonpool runs single-threaded so this is sound.
    struct SendCell(Cell<usize>);
    unsafe impl Send for SendCell {}
    unsafe impl Sync for SendCell {}

    #[test]
    fn cursor_since_returns_only_new_entries() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        let cursor = Arc::new(SendCell(Cell::new(0)));
        let cursor_clone = cursor.clone();
        let observed = Arc::new(AtomicUsize::new(0));
        let observed_clone = observed.clone();
        handle.register(invariant_fn("counter", move |q, _t| {
            let new = q.since::<Heartbeat>("hb", &cursor_clone.0);
            observed_clone.fetch_add(new.len(), Ordering::Relaxed);
        }));

        tracing::info!(
            capture = true,
            trail = "hb",
            event = valuable(&heartbeat("n", 1))
        );
        tracing::info!(
            capture = true,
            trail = "hb",
            event = valuable(&heartbeat("n", 2))
        );

        // 2 events, but each invariant call sees only the NEW entries past
        // the cursor: 1 then 1, total 2.
        assert_eq!(observed.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn reset_for_seed_clears_state() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        tracing::info!(
            capture = true,
            trail = "hb",
            event = valuable(&heartbeat("n", 1))
        );
        handle.reset_for_seed();
        tracing::info!(
            capture = true,
            trail = "hb",
            event = valuable(&heartbeat("n", 2))
        );

        let entries = handle.trail::<Heartbeat>("hb");
        assert_eq!(entries.len(), 1, "reset cleared the first event");
        assert_eq!(entries[0].seq, 0, "seq counter resets per seed");
        assert_eq!(handle.current_sim_time_ms(), 0, "clock resets per seed");
    }

    #[test]
    fn invariants_run_after_each_event_with_sim_time() {
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        let last_time = Arc::new(AtomicU64::new(u64::MAX));
        let last_time_clone = last_time.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        handle.register(invariant_fn("witness", move |_q, t| {
            last_time_clone.store(t, Ordering::Relaxed);
            calls_clone.fetch_add(1, Ordering::Relaxed);
        }));

        handle.set_sim_time_ms(100);
        tracing::info!(
            capture = true,
            trail = "hb",
            event = valuable(&heartbeat("n", 1))
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(last_time.load(Ordering::Relaxed), 100);

        handle.set_sim_time_ms(250);
        tracing::info!(
            capture = true,
            trail = "hb",
            event = valuable(&heartbeat("n", 2))
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(last_time.load(Ordering::Relaxed), 250);
    }

    #[test]
    fn invariant_emitting_event_is_silently_dropped() {
        // tracing-core's dispatch reentrancy guard suppresses events emitted
        // from inside another event's processing. So an invariant that emits
        // captured events is a no-op — no deadlock, no double-capture, no
        // panic. We document and lock in this behavior.
        let layer = SimulationLayer::new();
        let (handle, _guard) = layer.install();

        handle.register(invariant_fn("noisy", |_q, _t| {
            tracing::info!(
                capture = true,
                trail = "from_invariant",
                event = valuable(&heartbeat("from_inv", 99)),
            );
        }));

        tracing::info!(
            capture = true,
            trail = "outer",
            event = valuable(&heartbeat("outer", 1)),
        );

        assert_eq!(handle.trail::<Heartbeat>("outer").len(), 1);
        assert!(handle.trail::<Heartbeat>("from_invariant").is_empty());
    }

    // ------------------------------------------------------------------
    // SimTime fmt-integration tests (formatter, not the capture path)
    // ------------------------------------------------------------------

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

        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_timer(SimTime::new(handle.clone()))
            .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
        let subscriber = tracing_subscriber::registry().with(layer).with(fmt_layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("hello at 5s");
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
}
