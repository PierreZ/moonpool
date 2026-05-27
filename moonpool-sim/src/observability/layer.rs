//! Custom `tracing` layer that captures correctness events and runs invariants.
//!
//! [`SimulationLayer`] subscribes to ordinary `tracing` events. An event is
//! captured iff it carries `capture = true` as a structured field. Required
//! companion field: `trail = "..."` selects the named stream. Optional field:
//! `source = "..."` (defaults to empty string). The typed payload is carried
//! as `event = valuable(&p)` and converted to a [`serde_json::Value`] at
//! capture time.
//!
//! Simulation time is stamped from the layer's internal clock, advanced by
//! the orchestrator via [`SimulationLayerHandle::set_sim_time_ms`] between
//! events. In production (no orchestrator), the clock stays at 0 unless the
//! user pushes wall time themselves.
//!
//! Storage is held behind a [`parking_lot::Mutex`] to satisfy the
//! `Send + Sync` bound on `tracing::Layer`. moonpool simulations run
//! single-threaded so the mutex is uncontended.
//!
//! ## Invariants are read-only by construction
//!
//! `tracing-core`'s dispatch reentrancy guard silently drops any event emitted
//! while another event is being processed. So an invariant that calls
//! `tracing::info!(capture = true, ...)` from inside `observe(...)` is a no-op,
//! not a panic — there is no deadlock risk and no need for an explicit guard
//! in the layer.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use super::event::{CapturedEvent, TypedEntry};
use super::invariant::Invariant;
use super::query::TrailQuery;

#[allow(unused_imports)]
use super::query::TrailQueryExt;

/// Tracks how many [`SimulationLayer`]s are currently installed as a default
/// subscriber. Used by tests; not part of the capture path.
static INSTALL_COUNT: AtomicI32 = AtomicI32::new(0);

/// Returns true when at least one [`SimulationLayer`] is currently installed.
#[inline]
#[must_use]
pub fn layer_installed() -> bool {
    INSTALL_COUNT.load(Ordering::Relaxed) > 0
}

/// Mutable storage for captured events.
pub(crate) struct EventStore {
    /// Monotonic sequence counter assigned to each captured event.
    seq_counter: u64,
    /// Captured events grouped by their trail name.
    by_trail: HashMap<String, Vec<CapturedEvent>>,
    /// Latest known sim time. Advanced by `SimulationLayerHandle::set_sim_time_ms`
    /// (orchestrator pushes after each `sim.step()`) and read at capture time.
    last_sim_time_ms: u64,
}

impl EventStore {
    fn new() -> Self {
        Self {
            seq_counter: 0,
            by_trail: HashMap::new(),
            last_sim_time_ms: 0,
        }
    }
}

/// A `tracing::Layer` that captures `capture = true` events and pumps invariants.
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
        INSTALL_COUNT.fetch_add(1, Ordering::Relaxed);
        let subscriber = tracing_subscriber::registry().with(self);
        let guard = tracing::subscriber::set_default(subscriber);
        (handle, InstallGuard { _guard: guard })
    }
}

impl Default for SimulationLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Drop-guard returned by [`SimulationLayer::install`]. Restores the previous
/// subscriber and decrements the install count when dropped.
pub struct InstallGuard {
    _guard: tracing::subscriber::DefaultGuard,
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        INSTALL_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Cheap-to-clone handle to a [`SimulationLayer`]'s captured state.
#[derive(Clone)]
pub struct SimulationLayerHandle {
    events: Arc<Mutex<EventStore>>,
    invariants: Arc<Mutex<Vec<Box<dyn Invariant + Send>>>>,
}

impl SimulationLayerHandle {
    /// Register an invariant. Subsequently called after every captured event.
    pub fn register(&self, inv: Box<dyn Invariant + Send>) {
        self.invariants.lock().push(inv);
    }

    /// Reset captured events and invariant state for a new seed.
    ///
    /// Clears all event vectors, resets sequence counter, calls `Invariant::reset`
    /// on each registered invariant.
    pub fn reset_for_seed(&self) {
        {
            let mut store = self.events.lock();
            store.by_trail.clear();
            store.seq_counter = 0;
            store.last_sim_time_ms = 0;
        }
        let mut invs = self.invariants.lock();
        for inv in invs.iter_mut() {
            inv.reset();
        }
    }

    /// Read all events captured under `trail` as typed entries.
    ///
    /// Entries whose payload does not deserialize as `T` are skipped.
    pub fn trail<T: DeserializeOwned>(&self, trail: &str) -> Vec<TypedEntry<T>> {
        let store = self.events.lock();
        let Some(entries) = store.by_trail.get(trail) else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(TypedEntry::<T>::deserialize)
            .collect()
    }

    /// Latest known simulation time in milliseconds.
    ///
    /// Updated by [`Self::set_sim_time_ms`] (the orchestrator pushes after
    /// each `sim.step()`). Read at capture time to stamp the captured event.
    #[must_use]
    pub fn current_sim_time_ms(&self) -> u64 {
        self.events.lock().last_sim_time_ms
    }

    /// Override the latest known simulation time. Called by the orchestrator
    /// to keep the layer's clock advancing between events.
    pub fn set_sim_time_ms(&self, ms: u64) {
        self.events.lock().last_sim_time_ms = ms;
    }
}

/// `TrailQuery` view backed by an [`EventStore`]. Created on the fly inside
/// `on_event` so invariants can read from the live store without holding the
/// invariants lock during their `observe` call.
struct LayerQuery {
    events: Arc<Mutex<EventStore>>,
}

impl TrailQuery for LayerQuery {
    fn len(&self, trail: &str) -> usize {
        self.events
            .lock()
            .by_trail
            .get(trail)
            .map_or(0, std::vec::Vec::len)
    }

    fn last_seq(&self) -> u64 {
        let store = self.events.lock();
        store.seq_counter.saturating_sub(1)
    }

    fn drain_since(&self, trail: &str, cursor: &Cell<usize>) -> Vec<CapturedEvent> {
        let store = self.events.lock();
        let Some(entries) = store.by_trail.get(trail) else {
            return Vec::new();
        };
        let len = entries.len();
        let from = cursor.get();
        if from >= len {
            return Vec::new();
        }
        let result: Vec<CapturedEvent> = entries[from..].to_vec();
        cursor.set(len);
        result
    }
}

/// Visitor that scans a `tracing::Event` for the capture marker and required
/// companion fields. After visiting, the populated fields tell the layer
/// whether and how to record the event.
struct CaptureVisitor {
    capture: bool,
    trail: Option<String>,
    source: String,
    payload: Option<Value>,
}

impl CaptureVisitor {
    fn new() -> Self {
        Self {
            capture: false,
            trail: None,
            source: String::new(),
            payload: None,
        }
    }
}

impl Visit for CaptureVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "capture" {
            self.capture = value;
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "trail" => self.trail = Some(value.to_owned()),
            "source" => value.clone_into(&mut self.source),
            _ => {}
        }
    }

    fn record_value(&mut self, field: &Field, value: valuable::Value<'_>) {
        if field.name() == "event" {
            // Serialize the Valuable into a serde_json::Value. valuable-serde's
            // Serializable wrapper bridges Valuable → serde::Serialize.
            let serializable = valuable_serde::Serializable::new(value);
            if let Ok(v) = serde_json::to_value(serializable) {
                self.payload = Some(v);
            }
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {
        // Other fields (e.g. tracing's auto-added `message`) are ignored.
    }
}

impl<S> Layer<S> for SimulationLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = CaptureVisitor::new();
        event.record(&mut visitor);
        if !visitor.capture {
            return;
        }
        let Some(trail) = visitor.trail else {
            return;
        };
        let Some(payload) = visitor.payload else {
            return;
        };

        // Phase 1: append the event under the events lock.
        let sim_time_ms;
        {
            let mut store = self.events.lock();
            let seq = store.seq_counter;
            store.seq_counter += 1;
            sim_time_ms = store.last_sim_time_ms;
            store
                .by_trail
                .entry(trail.clone())
                .or_default()
                .push(CapturedEvent {
                    trail,
                    time_ms: sim_time_ms,
                    source: visitor.source,
                    seq,
                    payload,
                });
        }

        // Phase 2: run each invariant. The invariants lock is held during the
        // call; the event store is released so LayerQuery can re-acquire it.
        // Any captured event an invariant tries to emit is silently dropped by
        // tracing-core's dispatch reentrancy guard.
        let query = LayerQuery {
            events: self.events.clone(),
        };
        let invariants = self.invariants.lock();
        for inv in invariants.iter() {
            inv.observe(&query, sim_time_ms);
        }
    }
}
