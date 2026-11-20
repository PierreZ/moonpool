//! Build a TLA+ trace from the captured timeline, for model-based trace
//! checking with [`moonpool_tla`].
//!
//! The timeline already holds every event of the run in one total order
//! (the global sequence number of each [`TraceEvent`]), so a trace is the
//! chosen events, merged by that order. A workload's `check()` hands the
//! trace to a [`moonpool_tla::TraceCheck`]:
//!
//! ```ignore
//! let mut trace = moonpool_sim::observability::tla::trace_from_timeline(
//!     ctx.observability(),
//!     &["rm_prepare", "tm_commit", "rm_rcv_commit"],
//! );
//! trace.define("TraceRM", Value::str_set(["r1", "r2", "r3"]));
//! let verdict = TraceCheck::new(spec_dir, "TwoPhaseTrace").check(&trace)?;
//! ```

use moonpool_tla::{Trace, Value};

use super::{FieldValue, TraceEvent, TraceQuery};

/// The record field that holds the source of an event (the `ip` of the
/// process or workload that emitted it, or `"sim"`), unless the event has a
/// field of that name.
pub const SOURCE_FIELD: &str = "source";

/// The events named `names`, in the order they happened, as a TLA+ trace.
///
/// Each event becomes a record with its name in the `event` field, its
/// source in the `source` field, and one field per `tracing` field:
/// booleans and integers stay so, an `f64` becomes a string, and an integer
/// beyond `i64` becomes a string too. An integer beyond TLC's 32 bits is
/// refused later, when the trace is written: record a large id as a string
/// (`%`).
#[must_use]
pub fn trace_from_timeline(query: &dyn TraceQuery, names: &[&str]) -> Trace {
    let mut events: Vec<TraceEvent> = names.iter().flat_map(|name| query.snapshot(name)).collect();
    events.sort_by_key(|event| event.seq);
    events.dedup_by_key(|event| event.seq);
    let mut trace = Trace::new();
    for event in &events {
        let mut fields: Vec<(String, Value)> = event
            .fields
            .iter()
            .map(|(key, value)| (key.clone(), field_value(value)))
            .collect();
        if !event.fields.contains_key(SOURCE_FIELD) {
            fields.push((SOURCE_FIELD.to_owned(), Value::from(event.source.as_str())));
        }
        trace.event(&event.name, fields);
    }
    trace
}

fn field_value(value: &FieldValue) -> Value {
    match value {
        FieldValue::Bool(v) => Value::Bool(*v),
        FieldValue::I64(v) => Value::Int(*v),
        FieldValue::U64(v) => {
            i64::try_from(*v).map_or_else(|_| Value::Str(v.to_string()), Value::Int)
        }
        FieldValue::F64(v) => Value::Str(v.to_string()),
        FieldValue::Str(v) => Value::Str(v.clone()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;

    use super::*;

    /// A timeline held in memory.
    struct Timeline(Vec<TraceEvent>);

    impl TraceQuery for Timeline {
        fn len(&self, name: &str) -> usize {
            self.snapshot(name).len()
        }

        fn since(&self, name: &str, cursor: &Cell<usize>) -> Vec<TraceEvent> {
            let all = self.snapshot(name);
            let out = all[cursor.get().min(all.len())..].to_vec();
            cursor.set(all.len());
            out
        }

        fn snapshot(&self, name: &str) -> Vec<TraceEvent> {
            self.0.iter().filter(|e| e.name == name).cloned().collect()
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = self.0.iter().map(|e| e.name.clone()).collect();
            names.sort();
            names.dedup();
            names
        }

        fn total(&self) -> usize {
            self.0.len()
        }
    }

    fn event(seq: u64, name: &str, fields: &[(&str, FieldValue)]) -> TraceEvent {
        TraceEvent {
            seq,
            time_ms: seq,
            source: "10.0.1.1".to_owned(),
            target: "test".to_owned(),
            level: tracing::Level::INFO,
            name: name.to_owned(),
            fields: fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn events_are_merged_by_sequence_and_unchosen_names_are_left_out() {
        let timeline = Timeline(vec![
            event(0, "b", &[]),
            event(1, "a", &[("n", FieldValue::I64(1))]),
            event(2, "noise", &[]),
            event(3, "b", &[("ok", FieldValue::Bool(true))]),
        ]);
        let trace = trace_from_timeline(&timeline, &["a", "b"]);
        let module = trace.to_module("Trace").expect("writable");
        assert_eq!(
            module,
            "---- MODULE Trace ----\n\
             LOCAL INSTANCE Integers\n\
             Trace == <<\n  \
             [event |-> \"b\", source |-> \"10.0.1.1\"],\n  \
             [event |-> \"a\", n |-> 1, source |-> \"10.0.1.1\"],\n  \
             [event |-> \"b\", ok |-> TRUE, source |-> \"10.0.1.1\"]\n\
             >>\n====\n"
        );
    }

    #[test]
    fn a_name_listed_twice_gives_each_event_once() {
        let timeline = Timeline(vec![event(0, "a", &[])]);
        assert_eq!(trace_from_timeline(&timeline, &["a", "a"]).len(), 1);
    }

    #[test]
    fn fields_are_converted() {
        let timeline = Timeline(vec![event(
            0,
            "a",
            &[
                ("big", FieldValue::U64(u64::MAX)),
                ("small", FieldValue::U64(7)),
                ("ratio", FieldValue::F64(0.5)),
                ("source", FieldValue::Str("r1".to_owned())),
            ],
        )]);
        let trace = trace_from_timeline(&timeline, &["a"]);
        let module = trace.to_module("Trace").expect("writable");
        assert!(
            module.contains("big |-> \"18446744073709551615\""),
            "{module}"
        );
        assert!(module.contains("small |-> 7"), "{module}");
        assert!(module.contains("ratio |-> \"0.5\""), "{module}");
        assert!(module.contains("source |-> \"r1\""), "{module}");
    }
}
