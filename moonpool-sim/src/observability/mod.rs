//! Trace-based observability for moonpool simulations.
//!
//! Processes and workloads emit ordinary `tracing` events — the exact same
//! instrumentation production observability consumes (`fmt`, OpenTelemetry,
//! Loki, ...). In simulation, [`SimulationLayer`] captures those events into
//! a timeline and registered [`Invariant`]s cross-validate them after every
//! simulation step. The point: anomalies like dual leaders or metastable
//! failures are detected in simulation with the same signals you would use
//! to find them in production — traces.
//!
//! # Emission convention
//!
//! Emit a plain `tracing` event with a constant message (the event name) and
//! structured fields. No special markers, no derives:
//!
//! ```ignore
//! tracing::info!(target: "raft", term, leader = %my_ip, "leader_elected");
//! ```
//!
//! - Use `%` (`Display`) for strings and IPs — `?` (`Debug`) on a `String`
//!   includes quotes.
//! - The message must be a constant name (`"leader_elected"`), not an
//!   interpolated sentence: events are grouped and queried by name.
//!
//! An event is captured iff it is `INFO` or more severe, carries a non-empty
//! message, and fires inside a process/workload span (the orchestrator wraps
//! every actor task in a span carrying its `ip`, which becomes the event's
//! `source`). In production, where no [`SimulationLayer`] is installed, the
//! same emission flows to whatever subscriber is configured.
//!
//! # Querying
//!
//! Invariants receive a [`TraceQuery`] and read events by name, extracting
//! typed fields by key — the same way you would query a production trace
//! store:
//!
//! ```ignore
//! fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
//!     for e in q.since("leader_elected", &self.cursor) {
//!         let term = e.u64("term");
//!         let leader = e.str("leader");
//!         // assert one leader per term...
//!     }
//! }
//! ```
//!
//! Sim-injected faults (partitions, kills, storage corruption) are recorded
//! by the runner under the [`crate::SIM_FAULT_EVENT_NAME`] name with a
//! `kind` field and `source = "sim"`, so invariants can correlate
//! application anomalies with infrastructure faults in one timeline.

pub mod event;
pub mod fmt;
pub mod init;
pub mod invariant;
pub mod layer;
pub mod query;

pub use event::{FieldValue, TraceEvent};
pub use fmt::{Clock, SimTime};
pub use init::init_sim_tracing;
pub use invariant::{Invariant, invariant_fn};
pub use layer::{InstallGuard, SimulationLayer, SimulationLayerHandle};
pub use query::TraceQuery;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn in_actor_span(ip: &str, f: impl FnOnce()) {
        let span = tracing::info_span!("process", ip = %ip);
        let _enter = span.enter();
        f();
    }

    // ------------------------------------------------------------------
    // Capture-path tests
    // ------------------------------------------------------------------

    #[test]
    fn emit_without_layer_is_safe() {
        // No layer installed: tracing::info! does not panic and there's
        // nothing to observe. We just check the macro compiles and runs.
        tracing::info!(term = 1_u64, leader = %"10.0.1.1", "leader_elected");
    }

    #[test]
    fn layer_captures_event_inside_actor_span() {
        let (handle, _guard) = SimulationLayer::new().install();

        handle.set_sim_time_ms(1234);
        in_actor_span("10.0.1.1", || {
            tracing::info!(term = 7_u64, leader = %"10.0.1.1", "leader_elected");
        });

        let entries = handle.snapshot("leader_elected");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "leader_elected");
        assert_eq!(e.source, "10.0.1.1");
        assert_eq!(e.seq, 0);
        assert_eq!(e.time_ms, 1234);
        assert_eq!(e.level, tracing::Level::INFO);
        assert_eq!(e.u64("term"), Some(7));
        assert_eq!(e.str("leader"), Some("10.0.1.1"));
    }

    #[test]
    fn event_outside_actor_span_is_dropped() {
        let (handle, _guard) = SimulationLayer::new().install();

        tracing::info!(term = 7_u64, "leader_elected");

        assert!(handle.snapshot("leader_elected").is_empty());
    }

    #[test]
    fn debug_level_is_dropped() {
        let (handle, _guard) = SimulationLayer::new().install();

        in_actor_span("10.0.1.1", || {
            tracing::debug!(term = 7_u64, "leader_elected");
            tracing::trace!(term = 8_u64, "leader_elected");
            tracing::warn!(term = 9_u64, "leader_elected");
        });

        let entries = handle.snapshot("leader_elected");
        assert_eq!(entries.len(), 1, "only the WARN event is captured");
        assert_eq!(entries[0].u64("term"), Some(9));
        assert_eq!(entries[0].level, tracing::Level::WARN);
    }

    #[test]
    fn event_without_message_is_dropped() {
        let (handle, _guard) = SimulationLayer::new().install();

        in_actor_span("10.0.1.1", || {
            tracing::info!(term = 7_u64);
        });

        // No message → no name to group under; nothing captured anywhere.
        assert_eq!(handle.len("leader_elected"), 0);
    }

    #[test]
    fn nearest_enclosing_span_wins() {
        let (handle, _guard) = SimulationLayer::new().install();

        in_actor_span("10.0.1.1", || {
            in_actor_span("10.0.1.2", || {
                tracing::info!("ping_sent");
            });
        });

        let entries = handle.snapshot("ping_sent");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "10.0.1.2", "innermost ip attributes");
    }

    #[test]
    fn field_accessors_extract_typed_values() {
        let (handle, _guard) = SimulationLayer::new().install();

        in_actor_span("10.0.1.1", || {
            tracing::info!(
                count = 5_u64,
                delta = -3_i64,
                ratio = 0.5_f64,
                ok = true,
                name = %"alpha",
                detail = ?vec![1, 2],
                "mixed_fields"
            );
        });

        let entries = handle.snapshot("mixed_fields");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.u64("count"), Some(5));
        assert_eq!(e.i64("count"), Some(5), "u64 readable as i64 when it fits");
        assert_eq!(e.i64("delta"), Some(-3));
        assert_eq!(e.u64("delta"), None, "negative i64 not readable as u64");
        assert!((e.f64("ratio").expect("ratio field") - 0.5).abs() < f64::EPSILON);
        assert_eq!(e.bool("ok"), Some(true));
        assert_eq!(e.str("name"), Some("alpha"), "% display value unquoted");
        assert_eq!(e.str("detail"), Some("[1, 2]"), "? debug formatting");
        assert_eq!(e.u64("missing"), None);
    }

    #[test]
    fn seq_is_monotonic_across_names() {
        let (handle, _guard) = SimulationLayer::new().install();

        in_actor_span("10.0.1.1", || {
            tracing::info!("event_a");
            tracing::info!("event_b");
            tracing::info!("event_a");
        });

        let a = handle.snapshot("event_a");
        let b = handle.snapshot("event_b");
        assert_eq!(a[0].seq, 0);
        assert_eq!(b[0].seq, 1);
        assert_eq!(a[1].seq, 2);
    }

    #[test]
    fn cursor_since_returns_only_new_entries() {
        let (handle, _guard) = SimulationLayer::new().install();

        let cursor = Cell::new(0);
        in_actor_span("10.0.1.1", || {
            tracing::info!(n = 1_u64, "hb");
        });
        assert_eq!(handle.since("hb", &cursor).len(), 1);
        assert!(handle.since("hb", &cursor).is_empty(), "cursor advanced");

        in_actor_span("10.0.1.1", || {
            tracing::info!(n = 2_u64, "hb");
        });
        let new = handle.since("hb", &cursor);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].u64("n"), Some(2));
    }

    #[test]
    fn reset_for_seed_clears_state() {
        let (handle, _guard) = SimulationLayer::new().install();

        in_actor_span("10.0.1.1", || {
            tracing::info!(n = 1_u64, "hb");
        });
        handle.set_sim_time_ms(500);
        handle.reset_for_seed();
        in_actor_span("10.0.1.1", || {
            tracing::info!(n = 2_u64, "hb");
        });

        let entries = handle.snapshot("hb");
        assert_eq!(entries.len(), 1, "reset cleared the first event");
        assert_eq!(entries[0].seq, 0, "seq counter resets per seed");
        assert_eq!(handle.current_sim_time_ms(), 0, "clock resets per seed");
    }

    #[test]
    fn run_invariants_pumps_registered_invariants() {
        let (handle, _guard) = SimulationLayer::new().install();

        let observed = Arc::new(AtomicUsize::new(0));
        let observed_clone = observed.clone();
        let cursor = Cell::new(0);
        handle.register(invariant_fn("counter", move |q, _t| {
            let new = q.since("hb", &cursor);
            observed_clone.fetch_add(new.len(), Ordering::Relaxed);
        }));

        in_actor_span("10.0.1.1", || {
            tracing::info!(n = 1_u64, "hb");
            tracing::info!(n = 2_u64, "hb");
        });
        assert_eq!(
            observed.load(Ordering::Relaxed),
            0,
            "invariants do not run inside tracing dispatch"
        );

        handle.run_invariants();
        assert_eq!(observed.load(Ordering::Relaxed), 2, "batched at pump time");

        handle.run_invariants();
        assert_eq!(observed.load(Ordering::Relaxed), 2, "cursor advanced");
    }

    #[test]
    fn record_sim_fault_lands_in_timeline() {
        use crate::chaos::{SIM_FAULT_EVENT_NAME, SimFaultEvent};

        let (handle, _guard) = SimulationLayer::new().install();

        handle.record_sim_fault(
            42,
            &SimFaultEvent::PartitionCreated {
                from: "10.0.1.1".to_owned(),
                to: "10.0.1.2".to_owned(),
            },
        );
        handle.record_sim_fault(
            43,
            &SimFaultEvent::ProcessForceKill {
                ip: "10.0.1.1".to_owned(),
            },
        );

        let entries = handle.snapshot(SIM_FAULT_EVENT_NAME);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].source, "sim");
        assert_eq!(entries[0].time_ms, 42);
        assert_eq!(entries[0].str("kind"), Some("partition_created"));
        assert_eq!(entries[0].str("from"), Some("10.0.1.1"));
        assert_eq!(entries[0].str("to"), Some("10.0.1.2"));
        assert_eq!(entries[1].str("kind"), Some("process_force_kill"));
        assert_eq!(entries[1].str("ip"), Some("10.0.1.1"));
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
