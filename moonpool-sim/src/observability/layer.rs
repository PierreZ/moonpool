//! Custom `tracing` layer that captures plain trace events for invariants.
//!
//! [`SimulationLayer`] subscribes to ordinary `tracing` events — the same
//! emissions production observability consumes. An event is captured iff:
//!
//! 1. its level is `INFO` or more severe, and
//! 2. it is emitted inside a span carrying an `ip` field (the orchestrator
//!    wraps every process and workload task in such a span), and
//! 3. it has a non-empty message, which becomes the event's name.
//!
//! Runner-injected fault events bypass tracing entirely: the orchestrator
//! drains them from the simulation engine and records them via
//! [`SimulationLayerHandle::record_sim_fault`] with `source = "sim"`.
//!
//! Simulation time is stamped from the layer's internal clock, advanced by
//! the orchestrator via [`SimulationLayerHandle::set_sim_time_ms`] after each
//! step. Invariants are run by the orchestrator through
//! [`SimulationLayerHandle::run_invariants`] — never from inside tracing
//! dispatch.
//!
//! Storage is held behind a [`parking_lot::Mutex`] to satisfy the
//! `Send + Sync` bound on `tracing::Layer`. moonpool simulations run
//! single-threaded so the mutex is uncontended.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use crate::chaos::{SIM_FAULT_EVENT_NAME, SimFaultEvent};

use super::event::{FieldValue, TraceEvent};
use super::invariant::Invariant;
use super::query::TraceQuery;

/// Mutable storage for captured events.
struct EventStore {
    /// Monotonic sequence counter assigned to each captured event.
    seq_counter: u64,
    /// Captured events grouped by their name (the tracing message).
    by_name: HashMap<String, Vec<TraceEvent>>,
    /// Latest known sim time. Advanced by `SimulationLayerHandle::set_sim_time_ms`
    /// (orchestrator pushes after each `sim.step()`) and read at capture time.
    last_sim_time_ms: u64,
}

impl EventStore {
    fn new() -> Self {
        Self {
            seq_counter: 0,
            by_name: HashMap::new(),
            last_sim_time_ms: 0,
        }
    }

    fn push(
        &mut self,
        time_ms: u64,
        source: String,
        target: String,
        level: tracing::Level,
        name: String,
        fields: BTreeMap<String, FieldValue>,
    ) {
        let seq = self.seq_counter;
        self.seq_counter += 1;
        let event = TraceEvent {
            seq,
            time_ms,
            source,
            target,
            level,
            name: name.clone(),
            fields,
        };
        self.by_name.entry(name).or_default().push(event);
    }
}

/// A `tracing::Layer` that captures plain trace events emitted inside
/// process/workload spans.
pub struct SimulationLayer {
    events: Arc<Mutex<EventStore>>,
    invariants: Arc<Mutex<Vec<Box<dyn Invariant + Send>>>>,
}

impl SimulationLayer {
    /// Create a fresh layer with no events and no invariants.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(EventStore::new())),
            invariants: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Get a clonable handle for registering invariants and reading captured events.
    #[must_use]
    pub fn handle(&self) -> SimulationLayerHandle {
        SimulationLayerHandle {
            events: self.events.clone(),
            invariants: self.invariants.clone(),
        }
    }

    /// Install this layer as the per-thread default subscriber without any
    /// additional layers.
    #[must_use]
    pub fn install(self) -> (SimulationLayerHandle, InstallGuard) {
        let handle = self.handle();
        // Anchor: an inert, always-interested dispatcher kept alive for the
        // lifetime of the guard. `tracing` caches callsite interest globally and
        // recomputes it (against whichever dispatchers are currently live)
        // whenever that set changes. Without an anchor, a callsite can be
        // re-cached as `Interest::never` the moment the only live capturing
        // dispatcher's thread default is a `NoSubscriber` (a sibling test on
        // another thread, or a guard dropping mid-run), silently dropping our
        // events. Holding one dispatcher that votes "interested" for every
        // callsite guarantees the cache can never collapse to `never` while a
        // layer is installed. See issue #112.
        let interest_anchor = tracing::Dispatch::new(tracing_subscriber::registry());
        let subscriber = tracing_subscriber::registry().with(self);
        let guard = tracing::subscriber::set_default(subscriber);
        // `set_default` is thread-local and does not rebuild the interest cache,
        // so re-evaluate every callsite now against this capturing subscriber to
        // clear any stale `never` left by a callsite first hit before install.
        tracing::callsite::rebuild_interest_cache();
        (
            handle,
            InstallGuard {
                _guard: guard,
                _interest_anchor: interest_anchor,
            },
        )
    }
}

impl Default for SimulationLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Drop-guard returned by [`SimulationLayer::install`]. Restores the previous
/// subscriber when dropped.
pub struct InstallGuard {
    _guard: tracing::subscriber::DefaultGuard,
    /// Inert dispatcher kept alive so tracing's global callsite-interest cache
    /// cannot collapse callsites to `Interest::never` while a layer is
    /// installed. See `SimulationLayer::install`.
    _interest_anchor: tracing::Dispatch,
}

/// Cheap-to-clone handle to a [`SimulationLayer`]'s captured state.
#[derive(Clone)]
pub struct SimulationLayerHandle {
    events: Arc<Mutex<EventStore>>,
    invariants: Arc<Mutex<Vec<Box<dyn Invariant + Send>>>>,
}

impl SimulationLayerHandle {
    /// Register an invariant, run on every [`Self::run_invariants`] call.
    pub fn register(&self, inv: Box<dyn Invariant + Send>) {
        self.invariants.lock().push(inv);
    }

    /// Reset captured events and invariant state for a new seed.
    ///
    /// Clears all event vectors, resets the sequence counter, and calls
    /// `Invariant::reset` on each registered invariant.
    pub fn reset_for_seed(&self) {
        {
            let mut store = self.events.lock();
            store.by_name.clear();
            store.seq_counter = 0;
            store.last_sim_time_ms = 0;
        }
        let mut invs = self.invariants.lock();
        for inv in invs.iter_mut() {
            inv.reset();
        }
    }

    /// Latest known simulation time in milliseconds.
    ///
    /// Updated by [`Self::set_sim_time_ms`] (the orchestrator pushes after
    /// each `sim.step()`). Read at capture time to stamp captured events.
    #[must_use]
    pub fn current_sim_time_ms(&self) -> u64 {
        self.events.lock().last_sim_time_ms
    }

    /// Override the latest known simulation time. Called by the orchestrator
    /// to keep the layer's clock advancing between steps.
    pub fn set_sim_time_ms(&self, ms: u64) {
        self.events.lock().last_sim_time_ms = ms;
    }

    /// Record a runner-injected fault into the timeline.
    ///
    /// Stored under the [`SIM_FAULT_EVENT_NAME`] event name with
    /// `source = "sim"`, a `kind` field identifying the fault variant, and
    /// the fault's payload flattened into fields. `time_ms` is the sim time
    /// at which the fault occurred (stamped by the engine, not at drain time).
    pub fn record_sim_fault(&self, time_ms: u64, fault: &SimFaultEvent) {
        let mut fields = fault.to_fields();
        fields.insert("kind".to_owned(), FieldValue::Str(fault.kind().to_owned()));
        self.events.lock().push(
            time_ms,
            "sim".to_owned(),
            "moonpool_sim::fault".to_owned(),
            tracing::Level::INFO,
            SIM_FAULT_EVENT_NAME.to_owned(),
            fields,
        );
    }

    /// Run all registered invariants against the captured events at the
    /// current sim time. Called by the orchestrator after each step.
    pub fn run_invariants(&self) {
        let sim_time_ms = self.current_sim_time_ms();
        let invariants = self.invariants.lock();
        for inv in invariants.iter() {
            inv.observe(self, sim_time_ms);
        }
    }
}

impl TraceQuery for SimulationLayerHandle {
    fn len(&self, name: &str) -> usize {
        self.events
            .lock()
            .by_name
            .get(name)
            .map_or(0, std::vec::Vec::len)
    }

    fn since(&self, name: &str, cursor: &Cell<usize>) -> Vec<TraceEvent> {
        let store = self.events.lock();
        let Some(entries) = store.by_name.get(name) else {
            return Vec::new();
        };
        let len = entries.len();
        let from = cursor.get();
        if from >= len {
            return Vec::new();
        }
        let result: Vec<TraceEvent> = entries[from..].to_vec();
        cursor.set(len);
        result
    }

    fn snapshot(&self, name: &str) -> Vec<TraceEvent> {
        self.events
            .lock()
            .by_name
            .get(name)
            .cloned()
            .unwrap_or_default()
    }
}

/// Span-extension marker storing the `ip` field recorded on a process or
/// workload span. Read back at event time to attribute the event's source.
struct SourceIp(String);

/// Visitor that extracts an `ip` field from span attributes.
struct SpanIpVisitor {
    ip: Option<String>,
}

impl Visit for SpanIpVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "ip" {
            self.ip = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "ip" {
            self.ip = Some(format!("{value:?}"));
        }
    }
}

/// Visitor that collects an event's message (as its name) and structured
/// fields.
struct EventVisitor {
    name: Option<String>,
    fields: BTreeMap<String, FieldValue>,
}

impl EventVisitor {
    fn new() -> Self {
        Self {
            name: None,
            fields: BTreeMap::new(),
        }
    }
}

impl Visit for EventVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), FieldValue::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), FieldValue::I64(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), FieldValue::U64(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .insert(field.name().to_owned(), FieldValue::F64(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), FieldValue::Str(value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `%`-formatted (Display) values also land here, formatted without
        // quotes by tracing's DisplayValue wrapper.
        let formatted = format!("{value:?}");
        if field.name() == "message" {
            self.name = Some(formatted);
        } else {
            self.fields
                .insert(field.name().to_owned(), FieldValue::Str(formatted));
        }
    }
}

/// Walk the event's span scope from the innermost span outwards and return
/// the first recorded `ip`.
fn nearest_source<S>(ctx: &Context<'_, S>, event: &tracing::Event<'_>) -> Option<String>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let scope = ctx.event_scope(event)?;
    for span in scope {
        if let Some(ip) = span.extensions().get::<SourceIp>() {
            return Some(ip.0.clone());
        }
    }
    None
}

impl<S> Layer<S> for SimulationLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    // Report `sometimes` rather than caching a static interest, so tracing
    // re-checks `enabled` per event on the actual emitting thread. This keeps
    // capture correct under thread-local installs where the interest cache would
    // otherwise be decided once, globally, by whichever thread hit the callsite
    // first.
    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = SpanIpVisitor { ip: None };
        attrs.record(&mut visitor);
        if let Some(ip) = visitor.ip
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(SourceIp(ip));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        // 1. INFO or more severe only (Level::TRACE > Level::INFO in tracing's
        //    ordering).
        if *event.metadata().level() > tracing::Level::INFO {
            return;
        }
        // 2. Only events attributable to an actor span carrying an `ip`.
        let Some(source) = nearest_source(&ctx, event) else {
            return;
        };
        // 3. The message becomes the event name; drop unnamed events.
        let mut visitor = EventVisitor::new();
        event.record(&mut visitor);
        let Some(name) = visitor.name.filter(|n| !n.is_empty()) else {
            return;
        };

        let mut store = self.events.lock();
        let time_ms = store.last_sim_time_ms;
        store.push(
            time_ms,
            source,
            event.metadata().target().to_owned(),
            *event.metadata().level(),
            name,
            visitor.fields,
        );
    }
}
