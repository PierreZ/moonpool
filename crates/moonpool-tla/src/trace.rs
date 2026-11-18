//! A recorded trace and the TLA+ module that carries it to TLC.

use std::collections::BTreeMap;

use crate::value::{Value, ValueError, check_identifier};

/// The record field that holds the name of an event.
pub const EVENT_FIELD: &str = "event";

/// The events of one run, in the order they happened, plus the definitions
/// the trace spec needs to read them (the set of nodes, the highest ballot).
///
/// [`to_module`](Self::to_module) writes the trace as a TLA+ module that
/// defines `Trace`, a sequence of records. Each record has an `event` field
/// (the event's name) and one field per recorded value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trace {
    events: Vec<Value>,
    definitions: BTreeMap<String, Value>,
}

impl Trace {
    /// An empty trace.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one event. A field named `event` is replaced by `name`.
    pub fn event<K: Into<String>>(
        &mut self,
        name: &str,
        fields: impl IntoIterator<Item = (K, Value)>,
    ) {
        let mut record: BTreeMap<String, Value> =
            fields.into_iter().map(|(k, v)| (k.into(), v)).collect();
        record.insert(EVENT_FIELD.to_owned(), Value::Str(name.to_owned()));
        self.events.push(Value::Record(record));
    }

    /// Add `name == value` to the trace module. A trace spec uses a
    /// definition for what depends on the run, a set of node names say, and
    /// its TLC configuration gives it to a constant of the spec
    /// (`CONSTANT RM <- TraceRM`).
    pub fn define(&mut self, name: &str, value: Value) {
        self.definitions.insert(name.to_owned(), value);
    }

    /// How many events the trace holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the trace holds no event.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The events, each a [`Value::Record`].
    #[must_use]
    pub fn events(&self) -> &[Value] {
        &self.events
    }

    /// The first `len` events, with every definition.
    #[must_use]
    pub fn prefix(&self, len: usize) -> Self {
        Self {
            events: self.events.iter().take(len).cloned().collect(),
            definitions: self.definitions.clone(),
        }
    }

    /// Write the trace as the TLA+ module `module`: every definition, then
    /// `Trace`, one event per line.
    ///
    /// # Errors
    ///
    /// Returns the first value that cannot be written (see [`ValueError`]),
    /// or the module or definition name that is not a TLA+ identifier.
    pub fn to_module(&self, module: &str) -> Result<String, ValueError> {
        check_identifier(module)?;
        // `Integers` gives the unary minus of a negative value. A LOCAL
        // INSTANCE keeps its names out of the modules that extend the trace,
        // so a spec can define its own `Int` or `-`.
        let mut out = format!("---- MODULE {module} ----\nLOCAL INSTANCE Integers\n");
        for (name, value) in &self.definitions {
            check_identifier(name)?;
            out.push_str(name);
            out.push_str(" == ");
            value.write_tla(&mut out)?;
            out.push('\n');
        }
        out.push_str("Trace == <<");
        for (i, event) in self.events.iter().enumerate() {
            out.push_str(if i == 0 { "\n  " } else { ",\n  " });
            event.write_tla(&mut out)?;
        }
        out.push_str("\n>>\n====\n");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_events() -> Trace {
        let mut trace = Trace::new();
        trace.define("TraceRM", Value::str_set(["r1", "r2"]));
        trace.event("rm_prepare", [("rm", Value::from("r1"))]);
        trace.event("tm_commit", Vec::<(String, Value)>::new());
        trace
    }

    #[test]
    fn the_module_defines_the_trace_and_the_definitions() {
        let module = two_events().to_module("Trace").expect("writable");
        assert_eq!(
            module,
            "---- MODULE Trace ----\n\
             LOCAL INSTANCE Integers\n\
             TraceRM == {\"r1\", \"r2\"}\n\
             Trace == <<\n  \
             [event |-> \"rm_prepare\", rm |-> \"r1\"],\n  \
             [event |-> \"tm_commit\"]\n\
             >>\n====\n"
        );
    }

    #[test]
    fn an_empty_trace_is_the_empty_sequence() {
        let module = Trace::new().to_module("Trace").expect("writable");
        assert_eq!(
            module,
            "---- MODULE Trace ----\nLOCAL INSTANCE Integers\nTrace == <<\n>>\n====\n"
        );
    }

    #[test]
    fn a_prefix_keeps_the_definitions() {
        let trace = two_events();
        let prefix = trace.prefix(1);
        assert_eq!(prefix.len(), 1);
        assert!(!prefix.is_empty());
        assert!(
            prefix
                .to_module("Trace")
                .expect("writable")
                .contains("TraceRM ==")
        );
        assert_eq!(trace.prefix(9).len(), 2, "a prefix is never longer");
    }

    #[test]
    fn the_event_field_is_the_name() {
        let mut trace = Trace::new();
        trace.event("real", [(EVENT_FIELD, Value::from("spoofed"))]);
        assert!(
            trace
                .to_module("Trace")
                .expect("writable")
                .contains("[event |-> \"real\"]")
        );
    }

    #[test]
    fn a_bad_name_is_refused() {
        assert!(Trace::new().to_module("not a module").is_err());
        let mut trace = Trace::new();
        trace.define("a-b", Value::Int(1));
        assert!(trace.to_module("Trace").is_err());
    }
}
