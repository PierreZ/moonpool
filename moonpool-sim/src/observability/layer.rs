//! Custom `tracing` layer that captures simulation events and runs invariants.
//!
//! [`SimulationLayer`] subscribes to events with target `"moonpool::sim"`,
//! pulls their typed payloads from the per-thread emission stash, stores them
//! in a per-key vector, and runs all registered [`Invariant`]s.
//!
//! Storage is held behind a [`parking_lot::Mutex`] to satisfy the `Send + Sync`
//! bound on `tracing::Layer`. moonpool simulations run single-threaded so the
//! mutex is uncontended.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use super::emit::{PendingEmit, decrement_install_count, increment_install_count, take_pending};
use super::event::{CapturedEvent, TypedEntry};
use super::invariant::Invariant;
use super::query::TimelineQuery;

#[allow(unused_imports)]
use super::query::TimelineQueryExt;

/// Mutable storage for captured events.
pub(crate) struct EventStore {
    /// Monotonic sequence counter assigned to each captured event.
    seq_counter: u64,
    /// Captured events grouped by their static timeline key.
    by_key: HashMap<&'static str, Vec<CapturedEvent>>,
    /// Sim time of the most recent captured event.
    last_sim_time_ms: u64,
}

impl EventStore {
    fn new() -> Self {
        Self {
            seq_counter: 0,
            by_key: HashMap::new(),
            last_sim_time_ms: 0,
        }
    }
}

/// A `tracing::Layer` that captures simulation events and pumps invariants.
pub struct SimulationLayer {
    events: Arc<Mutex<EventStore>>,
    invariants: Arc<Mutex<Vec<Box<dyn Invariant + Send>>>>,
    /// Reentrancy guard — true while invariants are running.
    in_check: Arc<AtomicBool>,
}

impl SimulationLayer {
    /// Create a fresh layer with no events and no invariants.
    pub fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(EventStore::new())),
            invariants: Arc::new(Mutex::new(Vec::new())),
            in_check: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get a clonable handle for registering invariants and reading captured events.
    pub fn handle(&self) -> SimulationLayerHandle {
        SimulationLayerHandle {
            events: self.events.clone(),
            invariants: self.invariants.clone(),
            in_check: self.in_check.clone(),
        }
    }

    /// Install this layer as the per-thread default subscriber, composed with
    /// `additional` layers (e.g. `tracing_subscriber::fmt::layer()`).
    ///
    /// Returns `(handle, guard)` — the guard restores the previous subscriber
    /// when dropped. The handle outlives the guard for post-run inspection.
    pub fn install_with<L>(self, additional: L) -> (SimulationLayerHandle, InstallGuard)
    where
        L: Layer<tracing_subscriber::layer::Layered<SimulationLayer, tracing_subscriber::Registry>>
            + Send
            + Sync
            + 'static,
    {
        let handle = self.handle();
        increment_install_count();
        let subscriber = tracing_subscriber::registry().with(self).with(additional);
        let guard = tracing::subscriber::set_default(subscriber);
        (handle, InstallGuard { _guard: guard })
    }

    /// Install this layer as the per-thread default subscriber without any
    /// additional layers.
    ///
    /// Use [`Self::install_with`] to compose with `fmt`, OTel, or other layers.
    pub fn install(self) -> (SimulationLayerHandle, InstallGuard) {
        let handle = self.handle();
        increment_install_count();
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
        decrement_install_count();
    }
}

/// Cheap-to-clone handle to a [`SimulationLayer`]'s captured state.
#[derive(Clone)]
pub struct SimulationLayerHandle {
    events: Arc<Mutex<EventStore>>,
    invariants: Arc<Mutex<Vec<Box<dyn Invariant + Send>>>>,
    in_check: Arc<AtomicBool>,
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
            store.by_key.clear();
            store.seq_counter = 0;
            store.last_sim_time_ms = 0;
        }
        self.in_check.store(false, Ordering::Relaxed);
        let mut invs = self.invariants.lock();
        for inv in invs.iter_mut() {
            inv.reset();
        }
    }

    /// Total number of events captured across all timelines.
    pub fn snapshot_event_count(&self) -> usize {
        self.events.lock().by_key.values().map(|v| v.len()).sum()
    }

    /// Read all events captured under `key` as typed entries.
    ///
    /// Entries whose payload type does not match `T` are skipped.
    pub fn timeline<T: 'static + Clone>(&self, key: &'static str) -> Vec<TypedEntry<T>> {
        let store = self.events.lock();
        let Some(entries) = store.by_key.get(key) else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(typed_from_captured::<T>)
            .collect()
    }

    /// Number of events captured under `key`.
    pub fn timeline_len(&self, key: &'static str) -> usize {
        self.events
            .lock()
            .by_key
            .get(key)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Latest known simulation time in milliseconds.
    ///
    /// Updated automatically when an event is captured. The orchestrator also
    /// calls [`Self::set_sim_time_ms`] after each `sim.step()` so unrelated
    /// `tracing::*!` calls (with target other than `"moonpool::sim"`) emitted
    /// between events still see the current time.
    pub fn current_sim_time_ms(&self) -> u64 {
        self.events.lock().last_sim_time_ms
    }

    /// Override the latest known simulation time. Called by the orchestrator
    /// to keep the layer's clock advancing between events.
    pub fn set_sim_time_ms(&self, ms: u64) {
        self.events.lock().last_sim_time_ms = ms;
    }
}

fn typed_from_captured<T: 'static + Clone>(e: &CapturedEvent) -> Option<TypedEntry<T>> {
    e.payload.as_any().downcast_ref::<T>().map(|t| TypedEntry {
        event: t.clone(),
        time_ms: e.time_ms,
        source: e.source.clone(),
        seq: e.seq,
    })
}

/// `TimelineQuery` view backed by an [`EventStore`]. Created on the fly inside
/// `on_event` so invariants can read from the live store without holding the
/// invariants lock during their `observe` call.
struct LayerQuery {
    events: Arc<Mutex<EventStore>>,
}

impl TimelineQuery for LayerQuery {
    fn len(&self, key: &'static str) -> usize {
        self.events
            .lock()
            .by_key
            .get(key)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    fn last_seq(&self) -> u64 {
        let store = self.events.lock();
        store.seq_counter.saturating_sub(1)
    }

    fn drain_since(&self, key: &'static str, cursor: &Cell<usize>) -> Vec<CapturedEvent> {
        let store = self.events.lock();
        let Some(entries) = store.by_key.get(key) else {
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

impl<S> Layer<S> for SimulationLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "moonpool::sim" {
            return;
        }
        let Some(pending) = take_pending() else {
            return;
        };

        if self.in_check.load(Ordering::Relaxed) {
            panic!(
                "Invariant emitted a simulation event (forbidden — invariants must be read-only)"
            );
        }

        // Phase 1: append the event under the events lock.
        let sim_time_ms;
        {
            let mut store = self.events.lock();
            let PendingEmit {
                key,
                time_ms,
                source,
                payload,
            } = pending;
            let seq = store.seq_counter;
            store.seq_counter += 1;
            store.last_sim_time_ms = time_ms;
            sim_time_ms = time_ms;
            store.by_key.entry(key).or_default().push(CapturedEvent {
                key,
                time_ms,
                source,
                seq,
                payload,
            });
        }

        // Phase 2: run each invariant. The invariants lock is held during the
        // call; the event store is released so LayerQuery can re-acquire it.
        self.in_check.store(true, Ordering::Relaxed);
        let query = LayerQuery {
            events: self.events.clone(),
        };
        let invariants = self.invariants.lock();
        for inv in invariants.iter() {
            inv.observe(&query, sim_time_ms);
        }
        drop(invariants);
        self.in_check.store(false, Ordering::Relaxed);
    }
}
